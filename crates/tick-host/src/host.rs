//! The Tick host service: one per user by default, holding each hosted Tick's PTY master,
//! terminal model and launcher. Only the Agent owner controls lifecycles; data clients read
//! through a per-instance token; launchers report through a one-time launch token.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tick_connect::{
    host_error::HostError,
    hosted::{
        self, ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_CAPABILITIES,
        BUSINESS_FRAME_BYTES, BUSINESS_MAJOR, BUSINESS_MINOR, CALL_TIMEOUT, CONTROL_TIMEOUT, Hello,
        HelloReply, HostedTerminal, LaunchBinding, MANAGEMENT_FRAME_BYTES, MANAGEMENT_MAJOR,
        MANAGEMENT_MINOR, Role, TerminalDevice, same_secret,
    },
    instance::InstanceRef,
    session::{SessionBackend, SessionClient},
};
use tokio::{
    io::BufReader,
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    process::{Child, Command},
    sync::{Mutex, watch},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    paths::{self, HostPaths},
    process,
    pty::{Pty, READ_CHUNK_BYTES, TerminalSize},
    screen::Screen,
};

/// A host that was never used exits after this grace.
pub const INITIAL_GRACE: Duration = Duration::from_secs(5);
pub const REAP_INTERVAL: Duration = Duration::from_millis(250);
/// A Tick nobody attaches to, watches or explicitly uses for this long is reaped.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// The start configuration the owner writes on the host's standard input.
const CONFIG_BYTES: u64 = 1024;
/// How long a host's emptied mark is kept for owners to settle that host's records.
const EMPTIED_MARK_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Polite termination before SIGKILL.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(3);
pub const RECEIPT_CAPACITY: usize = 4096;
pub const REAPED_RING: usize = 32;
pub const DIAGNOSTIC_CHARACTERS: usize = 256;
/// A stalled stream reader or attached view loses its connection and lease; the Tick and other
/// readers continue.
pub const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// The send buffer requested for an attached view; it holds one current frame at a time.
pub const ATTACH_SEND_BUFFER_BYTES: usize = 64 * 1024;
/// After the host finished stopping, how long a waiting `shutdown_host` may take to answer.
pub const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_secs(1);
/// A failed accept (such as a full descriptor table) is retried after this pause.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Random bytes in every key and token the host issues.
const TOKEN_BYTES: usize = 32;
const INHERITED_CONTEXT: [&str; 4] = [
    tick_connect::protocol::INSTANCE_ID_ENV,
    hosted::LAUNCH_ENV,
    tick_connect::session::RUNTIME_DIR_ENV,
    paths::RUNTIME_DIR_ENV,
];
const HERDR_ENVIRONMENT_PREFIX: &str = "HERDR_";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct TickOwner {
    /// The canonical config root identity the owner daemon verified for this Chat.
    pub root: String,
    pub chat: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRequest {
    pub operation: Uuid,
    pub owner: TickOwner,
    /// The owner registry revision this open was decided at; an owner ended at this revision or
    /// later cannot open.
    pub owner_revision: u64,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub environment: Vec<(String, String)>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OpenedTick {
    pub instance: InstanceRef,
    pub terminal: HostedTerminal,
    pub business_pid: u32,
    pub data_token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Receipt {
    /// The host accepted the operation; its outcome is not known yet.
    Dispatched,
    Opened(OpenedTick),
    Closed {
        instance: InstanceRef,
    },
    OwnerEnded {
        owner: TickOwner,
        instances: Vec<InstanceRef>,
    },
    Failed(HostError),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickRow {
    pub instance: Option<InstanceRef>,
    pub terminal: Uuid,
    /// The reader token of a running instance, for its owner's own data clients.
    pub data_token: Option<String>,
    pub owner: TickOwner,
    pub title: Option<String>,
    pub state: TickState,
    pub business_pid: Option<u32>,
    pub attached: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TickState {
    Opening,
    Running,
    Closing,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReapReason {
    /// The launcher exited by itself.
    Exited,
    Closed,
    /// The Tick never became ready or exited while starting.
    LaunchFailed,
    /// A complete owner snapshot no longer lists its Chat.
    Orphaned,
    OwnerEnded,
    /// Nobody attached, watched or explicitly used it for the idle timeout.
    Idle,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ReapedTick {
    pub terminal: Uuid,
    pub instance: Option<InstanceRef>,
    pub reason: ReapReason,
}

/// Whether a read is someone's explicit use of the Tick, which keeps it from idling, or an
/// automatic baseline or poll, which does not.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadActivity {
    Explicit,
    Automatic,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Resources {
    pub business_pid: u32,
    pub cpu_time_ms: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub sampled_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Lifecycle {
    Opening,
    Live {
        instance: InstanceRef,
        business_pid: u32,
    },
    Closing,
}

struct HostedTick {
    terminal: Uuid,
    operation: Uuid,
    owner: TickOwner,
    title: Option<String>,
    launch_token: String,
    /// Set by the first launcher connection: the launch token works once.
    launcher_claimed: AtomicBool,
    data_token: String,
    pty: Pty,
    screen: std::sync::Mutex<Screen>,
    launcher: Mutex<Child>,
    launcher_pid: u32,
    lifecycle: watch::Sender<Lifecycle>,
    session: Mutex<Option<SessionClient>>,
    /// The last open, explicit read or call, attached input, or removal of the last Watch
    /// intent; nothing automatic counts.
    last_activity_ms: AtomicU64,
    attached: AtomicBool,
    /// The approved Watches that keep this Tick, as its owner last reported them.
    intents: std::sync::Mutex<WatchIntents>,
    screen_changes: watch::Sender<()>,
    terminal_closed: CancellationToken,
}

/// Watch intents are a lease: each report renews them, and ones not renewed within the idle
/// timeout lapse, so a Watch whose worker or Loop is gone never keeps a Tick forever.
#[derive(Default)]
struct WatchIntents {
    watches: HashSet<Uuid>,
    renewed_ms: u64,
}

impl HostedTick {
    fn note_activity(&self) {
        self.last_activity_ms
            .store(Host::now_ms(), Ordering::SeqCst);
    }

    /// Drops intents that were not renewed in time; their lapse restarts the idle time.
    fn lapse_stale_intents(&self, now_ms: u64, idle_timeout: Duration) {
        let mut intents = self.intents.lock().unwrap();
        if !intents.watches.is_empty()
            && now_ms.saturating_sub(intents.renewed_ms) >= idle_timeout.as_millis() as u64
        {
            intents.watches.clear();
            self.note_activity();
        }
    }

    fn is_idle(&self, now_ms: u64, idle_timeout: Duration) -> bool {
        matches!(*self.lifecycle.borrow(), Lifecycle::Live { .. })
            && !self.attached.load(Ordering::SeqCst)
            && self.intents.lock().unwrap().watches.is_empty()
            && now_ms.saturating_sub(self.last_activity_ms.load(Ordering::SeqCst))
                >= idle_timeout.as_millis() as u64
    }
}

/// Operation receipts, newest kept, each readable only by the owner it was made for.
#[derive(Default)]
struct Receipts {
    entries: VecDeque<(Uuid, TickOwner, Receipt)>,
}

impl Receipts {
    fn record(&mut self, operation: Uuid, owner: &TickOwner, receipt: Receipt) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(known, recorded, _)| *known == operation && recorded == owner)
        {
            entry.2 = receipt;
            return;
        }
        if self.entries.len() == RECEIPT_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((operation, owner.clone(), receipt));
    }

    fn get(&self, operation: Uuid, owner: &TickOwner) -> Option<Receipt> {
        self.entries
            .iter()
            .find(|(known, recorded, _)| *known == operation && recorded == owner)
            .map(|(_, _, receipt)| receipt.clone())
    }
}

/// Bounded facts for diagnosis; the host keeps no ongoing log.
#[derive(Default)]
struct Diagnostics {
    last_error: Option<String>,
    errors: u64,
    reaped: VecDeque<ReapedTick>,
}

impl Diagnostics {
    fn note_error(&mut self, error: &str) {
        self.errors += 1;
        self.last_error = Some(error.chars().take(DIAGNOSTIC_CHARACTERS).collect());
    }

    fn note_reaped(&mut self, reaped: ReapedTick) {
        if self.reaped.len() == REAPED_RING {
            self.reaped.pop_front();
        }
        self.reaped.push_back(reaped);
    }
}

pub struct HostState {
    ticks: HashMap<Uuid, Arc<HostedTick>>,
    receipts: Receipts,
    pending_opens: usize,
    ever_used: bool,
    /// Decided under this lock when the host stays empty; no open starts after it.
    exiting: bool,
    controller_generation: u64,
    controller_epoch: Option<Uuid>,
    /// Per config root: the last applied owner snapshot revision and one being received.
    snapshots: HashMap<String, RootSnapshots>,
    /// Owners ended at a registry revision; their opens at that revision or earlier fail.
    ended: HashMap<TickOwner, u64>,
    diagnostics: Diagnostics,
}

#[derive(Default)]
struct RootSnapshots {
    applied_revision: Option<u64>,
    staged: Option<StagedSnapshot>,
}

/// A complete owner snapshot arrives in chunks and is applied only once every chunk arrived.
struct StagedSnapshot {
    revision: u64,
    chunks: u32,
    received: std::collections::BTreeSet<u32>,
    owners: HashSet<Uuid>,
}

/// What the owner starts a host with.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub controller_key: String,
    #[serde(rename = "idle_timeout_ms", with = "milliseconds")]
    pub idle_timeout: Duration,
}

mod milliseconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(duration.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

pub struct Host {
    paths: HostPaths,
    epoch: Uuid,
    controller_key: String,
    idle_timeout: Duration,
    started: Instant,
    state: Mutex<HostState>,
    shutdown: CancellationToken,
    /// The operations that asked this host to shut down.
    shutdown_requests: std::sync::Mutex<HashSet<Uuid>>,
    shutdown_state: watch::Sender<ShutdownState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShutdownState {
    Running,
    Stopping,
    Completed,
}

impl Host {
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }

    async fn record_receipt(&self, operation: Uuid, owner: &TickOwner, receipt: Receipt) {
        self.state
            .lock()
            .await
            .receipts
            .record(operation, owner, receipt);
    }

    async fn receipt(&self, operation: Uuid, owner: &TickOwner) -> Option<Receipt> {
        self.state.lock().await.receipts.get(operation, owner)
    }

    async fn tick_by_instance(&self, instance: &InstanceRef) -> Option<Arc<HostedTick>> {
        let state = self.state.lock().await;
        state
            .ticks
            .values()
            .find(|tick| {
                matches!(&*tick.lifecycle.borrow(), Lifecycle::Live { instance: live, .. } if live == instance)
            })
            .cloned()
    }

    fn terminal_binding(&self, terminal: Uuid) -> HostedTerminal {
        HostedTerminal {
            endpoint: self.paths.socket(),
            epoch: self.epoch,
            terminal,
        }
    }

    async fn open(self: &Arc<Self>, request: OpenRequest) -> Result<OpenedTick, HostError> {
        // One lock covers the receipt lookup, the exit decision and the dispatch record, so a
        // repeated operation never launches twice and no open starts in a host that is leaving.
        {
            let mut state = self.state.lock().await;
            if let Some(receipt) = state.receipts.get(request.operation, &request.owner) {
                return match receipt {
                    Receipt::Opened(opened) => Ok(opened),
                    Receipt::Failed(error) => Err(error),
                    _ => Err(HostError::after(
                        "OperationInProgress",
                        "this open is still in progress; query its receipt",
                    )),
                };
            }
            if self.shutdown.is_cancelled() || state.exiting {
                return Err(HostError::before(
                    "ShuttingDown",
                    "the Tick host is stopping",
                ));
            }
            if state
                .ended
                .get(&request.owner)
                .is_some_and(|ended| request.owner_revision <= *ended)
            {
                return Err(HostError::before(
                    "OwnerEnded",
                    "this owner was ended; it cannot open Ticks",
                ));
            }
            validate_launch(&request)?;
            state.pending_opens += 1;
            state.ever_used = true;
            state
                .receipts
                .record(request.operation, &request.owner, Receipt::Dispatched);
        }
        let result = self.launch(&request).await;
        self.state.lock().await.pending_opens -= 1;
        let receipt = match &result {
            Ok(opened) => Receipt::Opened(opened.clone()),
            Err(error) => {
                self.state
                    .lock()
                    .await
                    .diagnostics
                    .note_error(&error.message);
                Receipt::Failed(error.clone())
            }
        };
        self.record_receipt(request.operation, &request.owner, receipt)
            .await;
        result
    }

    async fn launch(self: &Arc<Self>, request: &OpenRequest) -> Result<OpenedTick, HostError> {
        let terminal = Uuid::new_v4();
        let size = TerminalSize::default();
        let (pty, slave) = Pty::open(size)
            .map_err(|error| HostError::before("TerminalUnavailable", error.to_string()))?;
        let launch_token = random_token();
        let binding = LaunchBinding {
            terminal: self.terminal_binding(terminal),
            operation: request.operation,
            launch_token: launch_token.clone(),
        };
        let mut command = Command::new(&request.command[0]);
        command
            .args(&request.command[1..])
            .current_dir(&request.cwd)
            .env_clear()
            .envs(
                request
                    .environment
                    .iter()
                    .filter(|(name, _)| {
                        !name.starts_with(HERDR_ENVIRONMENT_PREFIX)
                            && !INHERITED_CONTEXT.contains(&name.as_str())
                    })
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .env(
                hosted::LAUNCH_ENV,
                serde_json::to_string(&binding).expect("launch binding serializes"),
            )
            .env(
                tick_connect::session::RUNTIME_DIR_ENV,
                self.paths.sessions(),
            )
            .env("TERM", "xterm-256color");
        slave
            .attach(&mut command)
            .map_err(|error| HostError::before("TerminalUnavailable", error.to_string()))?;
        let launcher = command
            .spawn()
            .map_err(|error| HostError::before("LaunchFailed", error.to_string()))?;
        let launcher_pid = launcher.id().ok_or_else(|| {
            HostError::before("LaunchFailed", "the launcher exited before it started")
        })?;
        let tick = Arc::new(HostedTick {
            terminal,
            operation: request.operation,
            owner: request.owner.clone(),
            title: request.title.clone(),
            launch_token,
            launcher_claimed: AtomicBool::new(false),
            data_token: random_token(),
            pty,
            screen: std::sync::Mutex::new(Screen::new(size)),
            launcher: Mutex::new(launcher),
            launcher_pid,
            lifecycle: watch::channel(Lifecycle::Opening).0,
            session: Mutex::new(None),
            last_activity_ms: AtomicU64::new(Self::now_ms()),
            attached: AtomicBool::new(false),
            intents: std::sync::Mutex::new(WatchIntents::default()),
            screen_changes: watch::channel(()).0,
            terminal_closed: CancellationToken::new(),
        });
        self.state.lock().await.ticks.insert(terminal, tick.clone());
        tokio::spawn(drain_terminal(tick.clone()));
        let mut lifecycle = tick.lifecycle.subscribe();
        let live = tokio::time::timeout(CONTROL_TIMEOUT, async {
            loop {
                if let Lifecycle::Live {
                    instance,
                    business_pid,
                } = lifecycle.borrow_and_update().clone()
                {
                    return Some((instance, business_pid));
                }
                tokio::select! {
                    changed = lifecycle.changed() => if changed.is_err() { return None },
                    _ = tick.terminal_closed.cancelled() => return None,
                }
            }
        })
        .await;
        match live {
            Ok(Some((instance, business_pid))) => Ok(OpenedTick {
                instance,
                terminal: self.terminal_binding(terminal),
                business_pid,
                data_token: tick.data_token.clone(),
            }),
            outcome => {
                let screen = tick
                    .screen
                    .lock()
                    .unwrap()
                    .snapshot()
                    .lines
                    .join(" ")
                    .chars()
                    .take(DIAGNOSTIC_CHARACTERS)
                    .collect::<String>();
                self.terminate(&tick).await;
                self.reap(terminal, ReapReason::LaunchFailed).await;
                Err(HostError::before(
                    "LaunchFailed",
                    if outcome.is_err() {
                        format!(
                            "the Tick did not become ready within {CONTROL_TIMEOUT:?}: {screen}"
                        )
                    } else {
                        format!("the Tick exited during startup: {screen}")
                    },
                ))
            }
        }
    }

    async fn terminate(&self, tick: &HostedTick) {
        tick.lifecycle.send_replace(Lifecycle::Closing);
        stop_launcher(tick).await;
    }

    /// Ends an instance of `owner`. Another owner's instance is refused before anything happens;
    /// an instance that already ended is closed.
    async fn close(
        &self,
        operation: Uuid,
        owner: &TickOwner,
        instance: InstanceRef,
    ) -> Result<Receipt, HostError> {
        if let Some(receipt) = self.receipt(operation, owner).await
            && receipt != Receipt::Dispatched
        {
            return Ok(receipt);
        }
        let tick = self.tick_by_instance(&instance).await;
        if tick.as_ref().is_some_and(|tick| &tick.owner != owner) {
            return Err(HostError::before(
                "Forbidden",
                "the Tick belongs to another owner",
            ));
        }
        self.record_receipt(operation, owner, Receipt::Dispatched)
            .await;
        if let Some(tick) = tick {
            self.terminate(&tick).await;
            self.reap(tick.terminal, ReapReason::Closed).await;
        }
        let receipt = Receipt::Closed { instance };
        self.record_receipt(operation, owner, receipt.clone()).await;
        Ok(receipt)
    }

    /// Applies one chunk of a complete owner snapshot of `root`. Only the last chunk of a revision
    /// newer than the applied one reaps: Ticks of that root whose Chat is missing and that are
    /// not still opening. Partial, older or repeated snapshots never reap.
    async fn reconcile_owners(&self, snapshot: OwnerSnapshotChunk) -> Result<Value, HostError> {
        let OwnerSnapshotChunk {
            root,
            revision,
            chunk,
            chunks,
            owners,
        } = snapshot;
        if chunks == 0 || chunk >= chunks {
            return Err(HostError::before(
                "InvalidArguments",
                "a snapshot chunk must lie within its chunk count",
            ));
        }
        let orphans = {
            let mut state = self.state.lock().await;
            let snapshots = state.snapshots.entry(root.clone()).or_default();
            if snapshots
                .applied_revision
                .is_some_and(|applied| revision <= applied)
            {
                return Err(HostError::before(
                    "Fenced",
                    "a newer owner snapshot was already applied",
                ));
            }
            if !snapshots
                .staged
                .as_ref()
                .is_some_and(|staged| staged.revision == revision && staged.chunks == chunks)
            {
                snapshots.staged = Some(StagedSnapshot {
                    revision,
                    chunks,
                    received: Default::default(),
                    owners: HashSet::new(),
                });
            }
            let staged = snapshots.staged.as_mut().expect("staged above");
            staged.received.insert(chunk);
            staged.owners.extend(owners);
            if staged.received.len() < chunks as usize {
                return Ok(json!({"revision":revision,"applied":false}));
            }
            let alive = std::mem::take(&mut staged.owners);
            snapshots.staged = None;
            snapshots.applied_revision = Some(revision);
            state
                .ticks
                .values()
                .filter(|tick| {
                    tick.owner.root == root
                        && !alive.contains(&tick.owner.chat)
                        && matches!(*tick.lifecycle.borrow(), Lifecycle::Live { .. })
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let reaped = self.end_ticks(orphans, ReapReason::Orphaned).await;
        Ok(json!({"revision":revision,"applied":true,"reaped":reaped}))
    }

    /// Ends every Tick of `owner` and fences its later opens at `revision`.
    async fn end_owner(
        &self,
        owner: TickOwner,
        revision: u64,
        operation: Uuid,
    ) -> Result<Receipt, HostError> {
        let ticks = {
            let mut state = self.state.lock().await;
            if let Some(receipt) = state.receipts.get(operation, &owner)
                && receipt != Receipt::Dispatched
            {
                return Ok(receipt);
            }
            let ended = state.ended.entry(owner.clone()).or_default();
            *ended = (*ended).max(revision);
            state
                .receipts
                .record(operation, &owner, Receipt::Dispatched);
            state
                .ticks
                .values()
                .filter(|tick| tick.owner == owner)
                .cloned()
                .collect::<Vec<_>>()
        };
        let instances = self.end_ticks(ticks, ReapReason::OwnerEnded).await;
        let receipt = Receipt::OwnerEnded {
            owner: owner.clone(),
            instances,
        };
        self.record_receipt(operation, &owner, receipt.clone())
            .await;
        Ok(receipt)
    }

    /// Ends `ticks` side by side, so one that ignores SIGTERM delays none of the others.
    async fn end_ticks(&self, ticks: Vec<Arc<HostedTick>>, reason: ReapReason) -> Vec<InstanceRef> {
        let mut ended = Vec::new();
        let mut stopping = tokio::task::JoinSet::new();
        for tick in &ticks {
            if let Lifecycle::Live { instance, .. } =
                tick.lifecycle.send_replace(Lifecycle::Closing)
            {
                ended.push(instance);
            }
            let tick = tick.clone();
            stopping.spawn(async move { stop_launcher(&tick).await });
        }
        stopping.join_all().await;
        for tick in ticks {
            self.reap(tick.terminal, reason).await;
        }
        ended
    }

    async fn reap(&self, terminal: Uuid, reason: ReapReason) {
        let mut state = self.state.lock().await;
        let Some(tick) = state.ticks.remove(&terminal) else {
            return;
        };
        let instance = match &*tick.lifecycle.borrow() {
            Lifecycle::Live { instance, .. } => Some(instance.clone()),
            _ => None,
        };
        state.diagnostics.note_reaped(ReapedTick {
            terminal,
            instance,
            reason,
        });
        drop(state);
        tick.terminal_closed.cancel();
    }

    /// Replaces and renews the approved Watch intents of one of `owner`'s Ticks. Removing the
    /// last one restarts the Tick's idle time.
    async fn set_intents(
        &self,
        owner: &TickOwner,
        instance: &InstanceRef,
        watches: HashSet<Uuid>,
    ) -> Result<Value, HostError> {
        let tick = self
            .tick_by_instance(instance)
            .await
            .filter(|tick| &tick.owner == owner)
            .ok_or_else(|| HostError::before("TargetGone", "no such Tick of this owner"))?;
        let count = watches.len();
        let previous = std::mem::replace(
            &mut *tick.intents.lock().unwrap(),
            WatchIntents {
                watches,
                renewed_ms: Self::now_ms(),
            },
        )
        .watches;
        if count == 0 && !previous.is_empty() {
            tick.note_activity();
        }
        Ok(json!({"instance":instance,"watches":count}))
    }

    /// Reaps Ticks whose launcher exited or that stayed idle, and reports whether the host
    /// should exit as empty.
    async fn sweep(&self) -> bool {
        let ticks = self
            .state
            .lock()
            .await
            .ticks
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut idle = Vec::new();
        for tick in ticks {
            let exited = tick
                .launcher
                .try_lock()
                .is_ok_and(|mut launcher| !matches!(launcher.try_wait(), Ok(None)));
            if exited {
                self.reap(tick.terminal, ReapReason::Exited).await;
                continue;
            }
            tick.lapse_stale_intents(Self::now_ms(), self.idle_timeout);
            if tick.is_idle(Self::now_ms(), self.idle_timeout) {
                idle.push(tick);
            }
        }
        self.end_ticks(idle, ReapReason::Idle).await;
        let mut state = self.state.lock().await;
        state.exiting = state.ticks.is_empty()
            && state.pending_opens == 0
            && (state.ever_used || self.started.elapsed() >= INITIAL_GRACE);
        state.exiting
    }

    /// One page of the host's Ticks, optionally only those of one owner.
    async fn list(
        &self,
        owner: Option<&TickOwner>,
        cursor: Option<tick_connect::page::PageCursor>,
        limit: Option<usize>,
    ) -> Result<Value, HostError> {
        let rows = self
            .state
            .lock()
            .await
            .ticks
            .values()
            .filter(|tick| owner.is_none_or(|owner| &tick.owner == owner))
            .map(|tick| {
                let lifecycle = tick.lifecycle.borrow().clone();
                TickRow {
                    instance: match &lifecycle {
                        Lifecycle::Live { instance, .. } => Some(instance.clone()),
                        _ => None,
                    },
                    terminal: tick.terminal,
                    data_token: matches!(lifecycle, Lifecycle::Live { .. })
                        .then(|| tick.data_token.clone()),
                    owner: tick.owner.clone(),
                    title: tick.title.clone(),
                    state: match lifecycle {
                        Lifecycle::Opening => TickState::Opening,
                        Lifecycle::Live { .. } => TickState::Running,
                        Lifecycle::Closing => TickState::Closing,
                    },
                    business_pid: match lifecycle {
                        Lifecycle::Live { business_pid, .. } => Some(business_pid),
                        _ => None,
                    },
                    attached: tick.attached.load(Ordering::SeqCst),
                }
            })
            .collect::<Vec<_>>();
        let page = tick_connect::page::paginate(
            rows,
            |row| row.terminal.to_string(),
            cursor.as_ref(),
            limit,
        )
        .map_err(|error| HostError::before(error.code(), error.to_string()))?;
        let mut listed = json!(page);
        listed["epoch"] = json!(self.epoch);
        Ok(listed)
    }

    async fn diagnostics(&self) -> Value {
        let state = self.state.lock().await;
        json!({
            "epoch": self.epoch,
            "ticks": state.ticks.len(),
            "pending_opens": state.pending_opens,
            "errors": state.diagnostics.errors,
            "last_error": state.diagnostics.last_error,
            "reaped": state.diagnostics.reaped,
        })
    }

    async fn shut_down(&self) {
        let ticks = self
            .state
            .lock()
            .await
            .ticks
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut closing = tokio::task::JoinSet::new();
        for tick in ticks {
            closing.spawn(async move {
                stop_launcher(&tick).await;
                tick.terminal_closed.cancel();
            });
        }
        while closing.join_next().await.is_some() {}
        self.state.lock().await.ticks.clear();
    }
}

/// One chunk of a complete owner snapshot: every Chat of `root` that still owns hosted Ticks.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSnapshotChunk {
    pub root: String,
    pub revision: u64,
    pub chunk: u32,
    pub chunks: u32,
    pub owners: Vec<Uuid>,
}

/// Ends a launcher's process group: SIGTERM, up to three seconds, then SIGKILL. Only the group of
/// a launcher this host spawned and has not yet reaped is signalled.
async fn stop_launcher(tick: &HostedTick) {
    let mut launcher = tick.launcher.lock().await;
    if !matches!(launcher.try_wait(), Ok(None)) {
        return;
    }
    let group = tick.launcher_pid as libc::pid_t;
    unsafe { libc::killpg(group, libc::SIGTERM) };
    if tokio::time::timeout(TERMINATE_GRACE, launcher.wait())
        .await
        .is_err()
    {
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let _ = launcher.wait().await;
    }
}

/// Drains the PTY continuously so a hidden Tick never blocks on output.
async fn drain_terminal(tick: Arc<HostedTick>) {
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    loop {
        tokio::select! {
            _ = tick.terminal_closed.cancelled() => return,
            read = tick.pty.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    tick.screen.lock().unwrap().process(&buffer[..count]);
                    tick.screen_changes.send_replace(());
                    tokio::task::yield_now().await;
                }
            }
        }
    }
    tick.terminal_closed.cancel();
}

fn validate_launch(request: &OpenRequest) -> Result<(), HostError> {
    let program = request
        .command
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| HostError::before("InvalidArguments", "command is empty"))?;
    if !program.is_absolute() || !request.cwd.is_absolute() {
        return Err(HostError::before(
            "InvalidArguments",
            "executable and working directory must be absolute",
        ));
    }
    if !request.cwd.is_dir() {
        return Err(HostError::before(
            "InvalidArguments",
            "working directory does not exist",
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    if !program
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    {
        return Err(HostError::before(
            "InvalidArguments",
            format!("{} is not an executable file", program.display()),
        ));
    }
    Ok(())
}

pub fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    let file = std::fs::File::open("/dev/urandom").and_then(|mut file| {
        use std::io::Read;
        file.read_exact(&mut bytes)
    });
    file.expect("the system random source is readable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Runs the host until it is shut down or stays empty. The listener is bound only after the
/// exclusive lock is held, so a live host is never replaced.
pub async fn serve(paths: HostPaths, config: HostConfig) -> std::io::Result<()> {
    paths.prepare()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.lock())?;
    std::fs::set_permissions(
        paths.lock(),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another Tick host owns this runtime directory",
        ));
    }
    let socket = paths.socket();
    if std::fs::symlink_metadata(&socket).is_ok() {
        // The lock proves no live host serves this path; only a stale socket of ours remains.
        let metadata = std::fs::symlink_metadata(&socket)?;
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if !metadata.file_type().is_socket() || metadata.uid() != paths::current_uid() {
            return Err(std::io::Error::other(
                "the Tick host endpoint path is occupied by something else",
            ));
        }
        std::fs::remove_file(&socket)?;
    }
    paths.prune_emptied(EMPTIED_MARK_RETENTION);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let identity = paths::file_identity(&socket)?;
    let host = Arc::new(Host {
        paths,
        epoch: Uuid::new_v4(),
        controller_key: config.controller_key,
        idle_timeout: config.idle_timeout,
        started: Instant::now(),
        state: Mutex::new(HostState {
            ticks: HashMap::new(),
            receipts: Receipts::default(),
            pending_opens: 0,
            ever_used: false,
            exiting: false,
            snapshots: HashMap::new(),
            ended: HashMap::new(),
            controller_generation: 0,
            controller_epoch: None,
            diagnostics: Diagnostics::default(),
        }),
        shutdown: CancellationToken::new(),
        shutdown_requests: std::sync::Mutex::new(HashSet::new()),
        shutdown_state: watch::channel(ShutdownState::Running).0,
    });
    let mut sweep = tokio::time::interval(REAP_INTERVAL);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut emptied = false;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let host = host.clone();
                    tokio::spawn(async move { let _ = connection(stream, host).await; });
                }
                Err(_) => tokio::time::sleep(ACCEPT_RETRY_DELAY).await,
            },
            _ = sweep.tick() => {
                if host.sweep().await {
                    emptied = true;
                    break;
                }
            }
            _ = host.shutdown.cancelled() => break,
            _ = terminate.recv() => break,
        }
    }
    host.shutdown.cancel();
    host.shutdown_state.send_replace(ShutdownState::Stopping);
    host.shut_down().await;
    if emptied {
        // Without the mark an owner later counts this host's Ticks as lost rather than ended.
        let _ = host.paths.record_emptied(host.epoch);
    }
    paths::remove_if_identity(&socket, identity);
    host.shutdown_state.send_replace(ShutdownState::Completed);
    // Requesters waiting on a completed shutdown answer before the process leaves.
    let _ = tokio::time::timeout(SHUTDOWN_REPLY_GRACE, host.shutdown_state.closed()).await;
    drop(lock);
    Ok(())
}

async fn connection(stream: UnixStream, host: Arc<Host>) -> Result<(), HostError> {
    if hosted::peer_uid(&stream)? != paths::current_uid() {
        return Err(HostError::before("Forbidden", "peer is another user"));
    }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let hello = tokio::time::timeout(
        CONTROL_TIMEOUT,
        hosted::read_frame(&mut reader, MANAGEMENT_FRAME_BYTES),
    )
    .await
    .map_err(|_| HostError::before("Timeout", "hello timed out"))??
    .ok_or_else(|| HostError::before("Disconnected", "no hello"))?;
    let hello: Hello = match serde_json::from_value(hello) {
        Ok(hello) => hello,
        Err(error) => {
            let error = HostError::before("InvalidHello", error.to_string());
            let _ = hosted::write_frame(
                &mut writer,
                &hosted::error(&Value::Null, &error),
                MANAGEMENT_FRAME_BYTES,
            )
            .await;
            return Err(error);
        }
    };
    match hello {
        Hello::Management {
            major,
            controller_key,
            ..
        } => {
            if major != MANAGEMENT_MAJOR || !same_secret(&controller_key, &host.controller_key) {
                let error = HostError::before(
                    "Forbidden",
                    "management requires the frozen 1.0 protocol and the owner's controller key",
                );
                let _ = hosted::write_frame(
                    &mut writer,
                    &hosted::error(&Value::Null, &error),
                    MANAGEMENT_FRAME_BYTES,
                )
                .await;
                return Err(error);
            }
            hosted::write_frame(
                &mut writer,
                &hosted::ok(
                    &Value::Null,
                    json!(HelloReply {
                        epoch: host.epoch,
                        pid: std::process::id(),
                        major: MANAGEMENT_MAJOR,
                        minor: MANAGEMENT_MINOR,
                        capabilities: Vec::new(),
                    }),
                ),
                MANAGEMENT_FRAME_BYTES,
            )
            .await?;
            management(reader, writer, host).await
        }
        Hello::Business {
            major,
            required,
            role,
            ..
        } => {
            let refusal = if major != BUSINESS_MAJOR {
                Some(HostError::before(
                    "IncompatibleHost",
                    format!("Tick host serves business protocol {BUSINESS_MAJOR}.{BUSINESS_MINOR}"),
                ))
            } else {
                required
                    .iter()
                    .find(|capability| !BUSINESS_CAPABILITIES.contains(&capability.as_str()))
                    .map(|missing| {
                        HostError::before(
                            "IncompatibleHost",
                            format!("Tick host lacks the {missing} capability"),
                        )
                    })
            };
            let session = match refusal {
                Some(error) => Err(error),
                None => authenticate(&host, role).await,
            };
            let session = match session {
                Ok(session) => session,
                Err(error) => {
                    let _ = hosted::write_frame(
                        &mut writer,
                        &hosted::error(&Value::Null, &error),
                        BUSINESS_FRAME_BYTES,
                    )
                    .await;
                    return Err(error);
                }
            };
            hosted::write_frame(
                &mut writer,
                &hosted::ok(
                    &Value::Null,
                    json!(HelloReply {
                        epoch: host.epoch,
                        pid: std::process::id(),
                        major: BUSINESS_MAJOR,
                        minor: BUSINESS_MINOR,
                        capabilities: BUSINESS_CAPABILITIES
                            .iter()
                            .map(|value| (*value).to_owned())
                            .collect(),
                    }),
                ),
                BUSINESS_FRAME_BYTES,
            )
            .await?;
            business(reader, writer, host, session).await
        }
    }
}

enum Authority {
    Controller {
        generation: u64,
    },
    Instance(Arc<HostedTick>),
    /// The one connection of a pending launch; `verified` once it proved its foreground.
    Launcher {
        tick: Arc<HostedTick>,
        verified: bool,
    },
}

async fn authenticate(host: &Arc<Host>, role: Role) -> Result<Authority, HostError> {
    match role {
        Role::Controller {
            controller_key,
            owner_epoch,
        } => {
            if !same_secret(&controller_key, &host.controller_key) {
                return Err(HostError::before(
                    "Forbidden",
                    "only the Agent owner controls Tick lifecycles",
                ));
            }
            let mut state = host.state.lock().await;
            if state.controller_epoch != Some(owner_epoch) {
                // A new owner fences every earlier controller connection.
                state.controller_generation += 1;
                state.controller_epoch = Some(owner_epoch);
            }
            Ok(Authority::Controller {
                generation: state.controller_generation,
            })
        }
        Role::Instance {
            instance,
            data_token,
        } => host
            .tick_by_instance(&instance)
            .await
            .filter(|tick| same_secret(&data_token, &tick.data_token))
            .map(Authority::Instance)
            .ok_or_else(|| HostError::before("Forbidden", "unknown instance or data token")),
        Role::Launcher {
            operation,
            launch_token,
        } => host
            .state
            .lock()
            .await
            .ticks
            .values()
            .find(|tick| {
                tick.operation == operation
                    && *tick.lifecycle.borrow() == Lifecycle::Opening
                    && same_secret(&launch_token, &tick.launch_token)
            })
            .cloned()
            .filter(|tick| !tick.launcher_claimed.swap(true, Ordering::SeqCst))
            .map(|tick| Authority::Launcher {
                tick,
                verified: false,
            })
            .ok_or_else(|| HostError::before("Forbidden", "unknown or already used launch token")),
    }
}

async fn management(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: Arc<Host>,
) -> Result<(), HostError> {
    while let Some(request) = tokio::time::timeout(
        CONTROL_TIMEOUT,
        hosted::read_frame(&mut reader, MANAGEMENT_FRAME_BYTES),
    )
    .await
    .map_err(|_| HostError::before("Timeout", "management request timed out"))??
    {
        let id = request["id"].clone();
        let reply = management_request(&host, request).await;
        let frame = match reply {
            Ok(result) => hosted::ok(&id, result),
            Err(error) => hosted::error(&id, &error),
        };
        hosted::write_frame(&mut writer, &frame, MANAGEMENT_FRAME_BYTES).await?;
    }
    Ok(())
}

/// The frozen management 1.0 methods; each names the host epoch it means and an operation.
/// `shutdown_host` answers once every Tick ended and the endpoint is gone.
async fn management_request(host: &Host, request: Value) -> Result<Value, HostError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Target {
        epoch: Uuid,
        operation: Uuid,
    }
    let method = request["method"].as_str().unwrap_or_default();
    if !matches!(method, "shutdown_host" | "shutdown_status") {
        return Err(HostError::before(
            "MethodNotAllowed",
            "management accepts only shutdown_host and shutdown_status",
        ));
    }
    let Target { epoch, operation } = parameters(request["params"].clone())?;
    if epoch != host.epoch {
        return Err(HostError::before(
            "EpochChanged",
            "this endpoint now serves another Tick host process",
        ));
    }
    if method == "shutdown_status" {
        let requested = host.shutdown_requests.lock().unwrap().contains(&operation);
        let state = if requested {
            json!(*host.shutdown_state.borrow())
        } else {
            json!("unknown")
        };
        return Ok(json!({"operation":operation,"state":state}));
    }
    host.shutdown_requests.lock().unwrap().insert(operation);
    let mut state = host.shutdown_state.subscribe();
    host.shutdown.cancel();
    state
        .wait_for(|state| *state == ShutdownState::Completed)
        .await
        .map_err(|_| HostError::after("Disconnected", "the host stopped without reporting"))?;
    Ok(json!({"operation":operation,"state":ShutdownState::Completed}))
}

async fn business(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: Arc<Host>,
    mut authority: Authority,
) -> Result<(), HostError> {
    loop {
        let request = tokio::select! {
            request = hosted::read_frame(&mut reader, BUSINESS_FRAME_BYTES) => request,
            _ = host.shutdown.cancelled() => return Ok(()),
        };
        let request = match request {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(error) => {
                let _ = hosted::write_frame(
                    &mut writer,
                    &hosted::error(&Value::Null, &error),
                    BUSINESS_FRAME_BYTES,
                )
                .await;
                return Err(error);
            }
        };
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        if let Authority::Instance(tick) = &authority {
            match method.as_str() {
                "watch" => {
                    return stream_observations(reader, writer, &host, tick.clone(), &id).await;
                }
                "attach" => match AttachLease::acquire(tick, params) {
                    Ok(lease) => return attach_view(reader, writer, &host, lease, &id).await,
                    Err(error) => {
                        hosted::write_frame(
                            &mut writer,
                            &hosted::error(&id, &error),
                            BUSINESS_FRAME_BYTES,
                        )
                        .await?;
                        continue;
                    }
                },
                _ => {}
            }
        }
        let reply = match &mut authority {
            Authority::Controller { generation } => {
                controller_request(&host, *generation, &method, params).await
            }
            Authority::Instance(tick) => instance_request(tick, &method, params).await,
            Authority::Launcher { tick, verified } => {
                launcher_request(&host, tick, verified, &method, params).await
            }
        };
        let frame = match reply {
            Ok(result) => hosted::ok(&id, result),
            Err(error) => hosted::error(&id, &error),
        };
        hosted::write_frame(&mut writer, &frame, BUSINESS_FRAME_BYTES).await?;
    }
}

fn parameters<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, HostError> {
    serde_json::from_value(params)
        .map_err(|error| HostError::before("InvalidArguments", error.to_string()))
}

async fn controller_request(
    host: &Arc<Host>,
    generation: u64,
    method: &str,
    params: Value,
) -> Result<Value, HostError> {
    if host.state.lock().await.controller_generation != generation {
        return Err(HostError::before(
            "Fenced",
            "a newer Agent owner controls this Tick host",
        ));
    }
    match method {
        "open" => host
            .open(parameters(params)?)
            .await
            .map(|opened| json!(opened)),
        "receipt" => {
            #[derive(Deserialize)]
            struct Query {
                operation: Uuid,
                owner: TickOwner,
            }
            let Query { operation, owner } = parameters(params)?;
            Ok(json!({"operation":operation,"receipt":host.receipt(operation, &owner).await}))
        }
        "close" => {
            #[derive(Deserialize)]
            struct Close {
                operation: Uuid,
                owner: TickOwner,
                instance: InstanceRef,
            }
            let Close {
                operation,
                owner,
                instance,
            } = parameters(params)?;
            host.close(operation, &owner, instance)
                .await
                .map(|receipt| json!(receipt))
        }
        "list" => {
            #[derive(Deserialize)]
            struct List {
                owner: Option<TickOwner>,
                cursor: Option<tick_connect::page::PageCursor>,
                limit: Option<usize>,
            }
            let List {
                owner,
                cursor,
                limit,
            } = parameters(params)?;
            host.list(owner.as_ref(), cursor, limit).await
        }
        "diagnostics" => Ok(host.diagnostics().await),
        "set_intents" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Intents {
                owner: TickOwner,
                instance: InstanceRef,
                watches: HashSet<Uuid>,
            }
            let Intents {
                owner,
                instance,
                watches,
            } = parameters(params)?;
            host.set_intents(&owner, &instance, watches).await
        }
        "reconcile_owners" => host.reconcile_owners(parameters(params)?).await,
        "end_owner" => {
            #[derive(Deserialize)]
            struct End {
                owner: TickOwner,
                revision: u64,
                operation: Uuid,
            }
            let End {
                owner,
                revision,
                operation,
            } = parameters(params)?;
            host.end_owner(owner, revision, operation)
                .await
                .map(|receipt| json!(receipt))
        }
        _ => Err(HostError::before(
            "MethodNotAllowed",
            format!("the controller role has no {method:?} method"),
        )),
    }
}

async fn session(tick: &HostedTick) -> Result<SessionClient, HostError> {
    tick.session
        .lock()
        .await
        .clone()
        .ok_or_else(|| HostError::before("TargetGone", "the Tick is not running"))
}

fn session_error(error: tick_connect::session::SessionError) -> HostError {
    use tick_connect::session::SessionError;
    match error {
        SessionError::Busy => HostError::before("Busy", error.to_string()),
        SessionError::CancelledBeforeSend => HostError::before("Cancelled", error.to_string()),
        SessionError::OutcomeUnknown => HostError::after("OutcomeUnknown", error.to_string()),
        SessionError::Gone => HostError::after("TargetGone", error.to_string()),
        SessionError::ChildError(message) => HostError::after("ToolError", message),
        SessionError::Protocol(message) => HostError::before("ProtocolViolation", message),
        SessionError::Io(error) => HostError::before("TargetGone", error.to_string()),
    }
}

async fn instance_request(
    tick: &Arc<HostedTick>,
    method: &str,
    params: Value,
) -> Result<Value, HostError> {
    if tick.terminal_closed.is_cancelled() {
        return Err(HostError::before("TargetGone", "the Tick has ended"));
    }
    match method {
        "describe" => session(tick)
            .await?
            .describe()
            .await
            .map(|descriptor| json!(descriptor))
            .map_err(session_error),
        "read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Read {
                activity: ReadActivity,
            }
            let Read { activity } = parameters(params)?;
            if activity == ReadActivity::Explicit {
                tick.note_activity();
            }
            session(tick)
                .await?
                .read()
                .await
                .map(|read| json!(read))
                .map_err(session_error)
        }
        "call" => {
            #[derive(Deserialize)]
            struct Call {
                tool: String,
                args: Value,
            }
            let Call { tool, args } = parameters(params)?;
            tick.note_activity();
            let client = session(tick).await?;
            tokio::time::timeout(
                CALL_TIMEOUT,
                client.call(&tool, args, CancellationToken::new()),
            )
            .await
            .map_err(|_| {
                HostError::after("OutcomeUnknown", "the Tick Tool did not finish in time")
            })?
            .map_err(session_error)
        }
        "snapshot" => Ok(json!(tick.screen.lock().unwrap().snapshot())),
        "resources" => {
            let Lifecycle::Live { business_pid, .. } = tick.lifecycle.borrow().clone() else {
                return Err(HostError::before("TargetGone", "the Tick is not running"));
            };
            let sample = process::sample(business_pid);
            Ok(json!(Resources {
                business_pid,
                cpu_time_ms: sample.and_then(|sample| sample.cpu_time_ms),
                rss_bytes: sample.and_then(|sample| sample.rss_bytes),
                sampled_at_ms: Host::now_ms(),
            }))
        }
        _ => Err(HostError::before(
            "MethodNotAllowed",
            format!("an instance reader has no {method:?} method"),
        )),
    }
}

async fn launcher_request(
    host: &Arc<Host>,
    tick: &Arc<HostedTick>,
    verified: &mut bool,
    method: &str,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        // The launcher proves it runs on this Tick's PTY in its foreground process group.
        "verify" => {
            #[derive(Deserialize)]
            struct Verify {
                device: TerminalDevice,
                process_group: u32,
            }
            let Verify {
                device,
                process_group,
            } = parameters(params)?;
            let foreground = unsafe { libc::tcgetpgrp(tick.pty.descriptor()) };
            if device != tick.pty.device()
                || process_group != tick.launcher_pid
                || foreground != tick.launcher_pid as libc::pid_t
            {
                return Err(HostError::before(
                    "ForegroundMismatch",
                    "the launcher is not the foreground of its Tick terminal",
                ));
            }
            *verified = true;
            Ok(json!({"verified":true}))
        }
        "register" => {
            #[derive(Deserialize)]
            struct Register {
                instance: InstanceRef,
                business_pid: u32,
            }
            let Register {
                instance,
                business_pid,
            } = parameters(params)?;
            if !*verified || *tick.lifecycle.borrow() != Lifecycle::Opening {
                return Err(HostError::before(
                    "Forbidden",
                    "a launcher registers once, after proving its foreground",
                ));
            }
            if instance.hosted_run().is_none() {
                return Err(HostError::before(
                    "InvalidArguments",
                    "a hosted launcher registers a hosted: instance",
                ));
            }
            if process::sample(business_pid).map(|sample| sample.parent) != Some(tick.launcher_pid)
            {
                return Err(HostError::before(
                    "InvalidArguments",
                    "the business process is not this launcher's child",
                ));
            }
            let client = SessionClient::connect(
                instance.clone(),
                SessionBackend::Hosted(host.terminal_binding(tick.terminal)),
                Some(host.paths.sessions()),
            )
            .await
            .map_err(session_error)?;
            client.describe().await.map_err(session_error)?;
            *tick.session.lock().await = Some(client);
            // A Tick whose open already timed out and began closing never becomes Live.
            let registered = tick.lifecycle.send_if_modified(|lifecycle| {
                let opening = *lifecycle == Lifecycle::Opening;
                if opening {
                    *lifecycle = Lifecycle::Live {
                        instance,
                        business_pid,
                    };
                }
                opening
            });
            if !registered {
                return Err(HostError::before(
                    "TargetGone",
                    "the Tick stopped opening before it registered",
                ));
            }
            Ok(json!({"registered":true}))
        }
        _ => Err(HostError::before(
            "MethodNotAllowed",
            format!("a launcher has no {method:?} method"),
        )),
    }
}

/// Streams a Tick's latest observations. The reader sends nothing more: its EOF, or anything
/// it sends, ends the stream and releases the Tick Session watch even while the Tick is quiet.
async fn stream_observations(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: &Arc<Host>,
    tick: Arc<HostedTick>,
    id: &Value,
) -> Result<(), HostError> {
    use tokio::io::AsyncBufReadExt;
    let client = session(&tick).await?;
    let (read, received, mut observations) = client.watch().await.map_err(session_error)?;
    hosted::write_frame(
        &mut writer,
        &hosted::ok(id, json!({"read":read,"received":received})),
        BUSINESS_FRAME_BYTES,
    )
    .await?;
    loop {
        tokio::select! {
            changed = observations.changed() => if changed.is_err() { return Ok(()) },
            _ = reader.fill_buf() => return Ok(()),
            _ = tick.terminal_closed.cancelled() => {
                let _ = hosted::write_frame(&mut writer, &json!({"event":"gone"}), BUSINESS_FRAME_BYTES).await;
                return Ok(());
            }
            _ = host.shutdown.cancelled() => return Ok(()),
        }
        let observation = observations.borrow_and_update().clone();
        tokio::time::timeout(
            STREAM_WRITE_TIMEOUT,
            hosted::write_frame(
                &mut writer,
                &json!({"event":"observation","observation":observation}),
                BUSINESS_FRAME_BYTES,
            ),
        )
        .await
        .map_err(|_| HostError::after("SlowReader", "the stream reader fell behind"))??;
    }
}

/// The single controller lease of one Tick terminal, held by its attached view until EOF,
/// detach, a stalled write or the end of the Tick.
struct AttachLease {
    tick: Arc<HostedTick>,
}

impl AttachLease {
    fn acquire(tick: &Arc<HostedTick>, params: Value) -> Result<Self, HostError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Attach {
            size: Option<TerminalSize>,
        }
        let Attach { size } = parameters(params)?;
        if tick.terminal_closed.is_cancelled() {
            return Err(HostError::before("TargetGone", "the Tick has ended"));
        }
        if tick
            .attached
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(HostError::before(
                "Busy",
                "another view controls this Tick terminal and keeps it",
            ));
        }
        let lease = Self { tick: tick.clone() };
        if let Some(size) = size {
            lease.resize(size)?;
        }
        Ok(lease)
    }

    /// The model follows first, so the redraw the resize signal causes is parsed at the new size.
    fn resize(&self, size: TerminalSize) -> Result<(), HostError> {
        self.tick.screen.lock().unwrap().resize(size);
        self.tick.screen_changes.send_replace(());
        self.tick
            .pty
            .resize(size)
            .map_err(|error| HostError::after("TerminalUnavailable", error.to_string()))
    }

    async fn input(&self, data: &[u8]) -> Result<(), HostError> {
        if data.len() > ATTACH_INPUT_BYTES {
            return Err(HostError::before(
                "InputTooLarge",
                format!("input chunks are at most {ATTACH_INPUT_BYTES} bytes"),
            ));
        }
        self.tick.note_activity();
        self.tick
            .pty
            .write_all(data)
            .await
            .map_err(|error| HostError::after("TerminalUnavailable", error.to_string()))
    }
}

impl Drop for AttachLease {
    fn drop(&mut self) {
        self.tick.attached.store(false, Ordering::SeqCst);
    }
}

/// Serves one attached view: the current frame, then latest-only changes, while its input and
/// resizes reach the same terminal. Output and input run side by side so neither is cut off
/// mid-frame; whichever ends first releases the lease and the Tick keeps running.
async fn attach_view(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: &Arc<Host>,
    lease: AttachLease,
    id: &Value,
) -> Result<(), HostError> {
    let tick = lease.tick.clone();
    request_send_buffer(&writer, ATTACH_SEND_BUFFER_BYTES);
    let mut changes = tick.screen_changes.subscribe();
    hosted::write_frame(
        &mut writer,
        &hosted::ok(id, json!({"attached":true})),
        BUSINESS_FRAME_BYTES,
    )
    .await?;
    let output = async {
        let mut shown = None;
        loop {
            changes.borrow_and_update();
            let (bytes, screen) = tick.screen.lock().unwrap().frame_since(shown.as_ref());
            shown = Some(screen);
            if !bytes.is_empty() {
                tokio::time::timeout(
                    STREAM_WRITE_TIMEOUT,
                    hosted::write_frame(
                        &mut writer,
                        &json!({"event":"frame","data":String::from_utf8_lossy(&bytes)}),
                        BUSINESS_FRAME_BYTES,
                    ),
                )
                .await
                .map_err(|_| HostError::after("SlowReader", "the attached view fell behind"))??;
            }
            if changes.changed().await.is_err() {
                return Ok(());
            }
        }
    };
    let input = async {
        #[derive(Deserialize)]
        #[serde(tag = "method", content = "params", rename_all = "snake_case")]
        enum ViewRequest {
            Input { data: Vec<u8> },
            Resize { size: TerminalSize },
            Detach,
        }
        while let Some(request) =
            hosted::read_frame(&mut reader, ATTACH_REQUEST_FRAME_BYTES).await?
        {
            match parameters(request)? {
                ViewRequest::Input { data } => lease.input(&data).await?,
                ViewRequest::Resize { size } => lease.resize(size)?,
                ViewRequest::Detach => break,
            }
        }
        Ok(())
    };
    let outcome = tokio::select! {
        outcome = output => outcome,
        outcome = input => outcome,
        _ = tick.terminal_closed.cancelled() => Err(HostError::after("TargetGone", "the Tick has ended")),
        _ = host.shutdown.cancelled() => Ok(()),
    };
    drop(lease);
    if let Err(error) = &outcome
        && error.code != "SlowReader"
    {
        let event = if error.code == "TargetGone" {
            json!({"event":"gone"})
        } else {
            json!({"event":"error","error":error})
        };
        let _ = tokio::time::timeout(
            STREAM_WRITE_TIMEOUT,
            hosted::write_frame(&mut writer, &event, BUSINESS_FRAME_BYTES),
        )
        .await;
    }
    outcome
}

fn request_send_buffer(writer: &OwnedWriteHalf, bytes: usize) {
    let size = bytes as libc::c_int;
    // A refused size only leaves the system default; the write timeout still bounds the view.
    unsafe {
        libc::setsockopt(
            writer.as_ref().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
}

/// Reads the start configuration the owner writes on the host's standard input.
pub fn read_config(input: impl std::io::Read) -> std::io::Result<HostConfig> {
    use std::io::BufRead;
    const KEY_CHARACTERS: usize = TOKEN_BYTES * 2;
    let mut line = String::new();
    std::io::BufReader::new(input.take(CONFIG_BYTES)).read_line(&mut line)?;
    let config = serde_json::from_str::<HostConfig>(&line)
        .ok()
        .filter(|config| {
            config.controller_key.len() == KEY_CHARACTERS
                && config
                    .controller_key
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        })
        .ok_or_else(|| {
            std::io::Error::other("the Tick host needs its start configuration on standard input")
        })?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_configuration_needs_a_well_formed_key_and_idle_time() {
        let key = random_token();
        let config = read_config(
            format!("{{\"controller_key\":\"{key}\",\"idle_timeout_ms\":2000}}\n").as_bytes(),
        )
        .unwrap();
        assert_eq!(config.controller_key, key);
        assert_eq!(config.idle_timeout, Duration::from_secs(2));
        for refused in [
            format!("{key}\n"),
            "{\"controller_key\":\"short\",\"idle_timeout_ms\":2000}\n".to_owned(),
            format!("{{\"controller_key\":\"{key}\"}}\n"),
        ] {
            assert!(read_config(refused.as_bytes()).is_err(), "{refused}");
        }
    }

    fn owner() -> TickOwner {
        TickOwner {
            root: "root".into(),
            chat: Uuid::new_v4(),
        }
    }

    #[test]
    fn receipts_keep_the_newest_operations_for_their_own_owner() {
        let mut receipts = Receipts::default();
        let (mine, other) = (owner(), owner());
        let first = Uuid::new_v4();
        receipts.record(first, &mine, Receipt::Dispatched);
        assert_eq!(
            receipts.get(first, &other),
            None,
            "another owner sees nothing"
        );
        receipts.record(first, &other, Receipt::Dispatched);
        let failed = Receipt::Failed(HostError::before("LaunchFailed", "no"));
        receipts.record(first, &mine, failed.clone());
        assert_eq!(receipts.get(first, &mine), Some(failed));
        assert_eq!(
            receipts.get(first, &other),
            Some(Receipt::Dispatched),
            "the same operation id of another owner is its own receipt"
        );
        for _ in 0..RECEIPT_CAPACITY - 1 {
            receipts.record(Uuid::new_v4(), &mine, Receipt::Dispatched);
        }
        assert_eq!(receipts.entries.len(), RECEIPT_CAPACITY);
        assert_eq!(
            receipts.get(first, &mine),
            None,
            "the oldest receipt expired"
        );
    }

    #[test]
    fn diagnostics_count_every_error_but_keep_bounded_facts() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.note_error(&"界".repeat(DIAGNOSTIC_CHARACTERS * 2));
        diagnostics.note_error("short");
        diagnostics.note_error(&"x".repeat(DIAGNOSTIC_CHARACTERS + 1));
        assert_eq!(diagnostics.errors, 3);
        assert_eq!(
            diagnostics.last_error.as_ref().unwrap().chars().count(),
            DIAGNOSTIC_CHARACTERS
        );
        let terminals = (0..REAPED_RING + 5)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        for terminal in &terminals {
            diagnostics.note_reaped(ReapedTick {
                terminal: *terminal,
                instance: None,
                reason: ReapReason::Exited,
            });
        }
        assert_eq!(diagnostics.reaped.len(), REAPED_RING);
        assert_eq!(diagnostics.reaped[0].terminal, terminals[5]);
    }
}
