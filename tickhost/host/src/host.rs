//! The Tickhost service: one per user by default, holding each hosted Tick's PTY master,
//! terminal model and process: the launcher of a protocol Tick, or a terminal Tick's program
//! itself; and each Shell task a Worker submitted. Only the Agent daemon controls lifecycles and
//! grants spawns; data clients read through a per-instance token; launchers report through a
//! one-time launch token; spawners submit one task each through a one-time spawn grant; views
//! attach to a Tick's terminal or a Shell task's screen through a one-time view grant.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
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
use tick_protocol::InstanceRef;
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        self, ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUILD_VERSION, BUSINESS_CAPABILITIES,
        BUSINESS_FRAME_BYTES, BUSINESS_MAJOR, BUSINESS_MINOR, CALL_TIMEOUT, CONTROL_TIMEOUT, Hello,
        HelloReply, HostedTerminal, LaunchBinding, MANAGEMENT_FRAME_BYTES, MANAGEMENT_MAJOR,
        MANAGEMENT_MINOR, Role, SPAWN_DESCRIPTORS, TerminalDevice, same_secret,
    },
    session::SessionClient,
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
    shell::{
        CANCELLED_REASON, ShellIdentity, ShellTask, SpawnBinding, SpawnGrant, SpawnGrants,
        SpawnRequest, SpawnedShell,
    },
    views::{ViewGrants, Views},
};

/// A host that was never used exits after this grace.
pub const INITIAL_GRACE: Duration = Duration::from_secs(5);
pub const REAP_INTERVAL: Duration = Duration::from_millis(250);
/// A Tick nobody attaches to, watches or explicitly uses for this long is reaped.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// The start configuration the daemon writes on the host's standard input.
const CONFIG_BYTES: u64 = 1024;
/// How long a host's emptied mark is kept for owners to settle that host's records.
const EMPTIED_MARK_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Polite termination before SIGKILL.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(3);
/// The receipts kept; one of an unfinished operation, or of an open whose Tick still runs, is
/// kept even beyond it.
pub const RECEIPT_CAPACITY: usize = 4096;
pub const REAPED_RING: usize = 32;
pub const DIAGNOSTIC_CHARACTERS: usize = 256;
/// The longest title the host keeps and lists, so a page of rows stays within its frame however
/// long the commands it names. The launch itself always gets the whole command.
pub const TITLE_CHARACTERS: usize = 256;
/// Ends a title that was cut.
pub const TITLE_ELLIPSIS: char = '…';
/// A stalled stream reader or attached view loses its connection; the Tick, its other views and
/// other readers continue.
pub const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// The send buffer requested for an attached view; it holds one current frame at a time.
pub const ATTACH_SEND_BUFFER_BYTES: usize = 64 * 1024;
/// How long a terminal Tick's program may hold off input written for it before the write is
/// abandoned, well within the reader's control timeout.
const INPUT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the host looks whether a terminal Tick's program ended, without waiting for it.
const EXIT_POLL: Duration = Duration::from_millis(50);
/// After the host finished stopping, how long a waiting `shutdown_host` may take to answer.
pub const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_secs(1);
/// A failed accept (such as a full descriptor table) is retried after this pause.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Random bytes in every key and token the host issues.
const TOKEN_BYTES: usize = 32;
const INHERITED_CONTEXT: [&str; 4] = [
    tick_protocol::INSTANCE_ID_ENV,
    hosted::LAUNCH_ENV,
    tickhost_connect::session::RUNTIME_DIR_ENV,
    paths::RUNTIME_DIR_ENV,
];
const HERDR_ENVIRONMENT_PREFIX: &str = "HERDR_";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct TickOwner {
    /// The canonical config root identity the daemon verified for this Chat.
    pub root: String,
    pub chat: Uuid,
}

/// How the host runs a Tick and offers it to readers.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TickAdapter {
    /// A Tick protocol program. Its command is a hosted launcher, which registers the Tick once
    /// it answered; the Tick describes, reads, calls and updates through the protocol.
    Protocol,
    /// A program without the Tick protocol, run directly on the terminal and Live as soon as it
    /// started: its screen text is its read, and its input is what it is called with.
    Terminal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRequest {
    pub operation: Uuid,
    pub owner: TickOwner,
    /// The owner registry revision this open was decided at; an owner ended at this revision or
    /// later cannot open.
    pub owner_revision: u64,
    pub adapter: TickAdapter,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub environment: Vec<(String, String)>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OpenedTick {
    pub instance: InstanceRef,
    pub adapter: TickAdapter,
    pub terminal: HostedTerminal,
    pub business_pid: u32,
    pub data_token: String,
}

/// What a terminal Tick runs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TerminalDescription {
    pub title: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

/// A terminal Tick's screen as plain text and, once its program exited, how it ended.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TerminalRead {
    /// Counts the screen's changes; a later read never has a smaller one.
    pub seq: u64,
    /// When the screen was read.
    pub at: String,
    pub text: String,
    /// Older lines were left out to keep within the snapshot bounds.
    pub truncated: bool,
    pub exit: Option<ProgramExit>,
}

impl TerminalRead {
    /// The content a reader compares and an Agent is given: the text and how the program ended,
    /// never styles or the cursor.
    pub fn data(&self) -> Value {
        json!({"text":self.text,"exit":self.exit})
    }
}

/// How a terminal Tick's program ended: its exit code, or the signal that ended it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProgramExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
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
    pub adapter: TickAdapter,
    pub terminal: Uuid,
    /// The reader token of a running instance, for its owner's own data clients.
    pub data_token: Option<String>,
    pub owner: TickOwner,
    pub title: Option<String>,
    pub state: TickState,
    pub business_pid: Option<u32>,
    /// How many views are attached to its terminal.
    pub views: usize,
    /// How a terminal Tick's program ended, once it did.
    pub exit: Option<ProgramExit>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TickState {
    Opening,
    Running,
    /// A terminal Tick whose program ended; its last screen stays readable until it is closed.
    Exited,
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

impl Lifecycle {
    fn live_instance(&self) -> Option<&InstanceRef> {
        match self {
            Self::Live { instance, .. } => Some(instance),
            _ => None,
        }
    }
}

struct HostedTick {
    terminal: Uuid,
    operation: Uuid,
    owner: TickOwner,
    title: Option<String>,
    adapter: Adapter,
    data_token: String,
    pty: Pty,
    screen: std::sync::Mutex<Screen>,
    /// Counts the changes of the screen: output parsed, a resize, the program's exit. It only
    /// changes, and is only read, under the screen's lock, so one revision shows one screen.
    screen_revision: AtomicU64,
    /// The process the host spawned: a protocol Tick's launcher, or a terminal Tick's program.
    /// A terminal Tick's program is only waited for once it is stopped, so its process group id
    /// stays its own until then.
    launcher: Mutex<Child>,
    launcher_pid: u32,
    lifecycle: watch::Sender<Lifecycle>,
    /// The last open, explicit read or call, attached input, or removal of the last Watch
    /// intent; nothing automatic counts.
    last_activity_ms: AtomicU64,
    /// The views attached to its terminal; any one keeps the Tick from idling.
    views: std::sync::Mutex<Views>,
    /// The views its owner allowed to attach that have not connected yet.
    view_grants: std::sync::Mutex<ViewGrants>,
    /// The approved Watches that keep this Tick, as its owner last reported them.
    intents: std::sync::Mutex<WatchIntents>,
    screen_changes: watch::Sender<()>,
    terminal_closed: CancellationToken,
}

/// What a hosted Tick holds for its adapter.
enum Adapter {
    Protocol(ProtocolLaunch),
    Terminal(TerminalProgram),
}

struct ProtocolLaunch {
    launch_token: String,
    /// Set by the first launcher connection: the launch token works once.
    launcher_claimed: AtomicBool,
    /// Where its launcher binds the Tick Session socket.
    session_directory: PathBuf,
    session: Mutex<Option<SessionClient>>,
}

struct TerminalProgram {
    command: Vec<String>,
    cwd: PathBuf,
    /// Set once the program exited and its terminal closed; its last screen stays readable until
    /// the Tick ends. It only changes under the screen's lock.
    exit: std::sync::Mutex<Option<ProgramExit>>,
}

impl TerminalProgram {
    fn exit(&self) -> Option<ProgramExit> {
        *self.exit.lock().unwrap()
    }
}

impl Adapter {
    fn kind(&self) -> TickAdapter {
        match self {
            Self::Protocol(_) => TickAdapter::Protocol,
            Self::Terminal(_) => TickAdapter::Terminal,
        }
    }
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

    /// Removes a protocol Tick's session directory once the launcher is gone, with any socket it
    /// could not remove itself.
    fn remove_session_directory(&self) {
        if let Adapter::Protocol(launch) = &self.adapter {
            let _ = std::fs::remove_dir_all(&launch.session_directory);
        }
    }

    /// Applies `change` to the screen and, when it says the screen changed, counts a new
    /// revision under the same lock and tells the attached views.
    fn change_screen(&self, change: impl FnOnce(&mut Screen) -> bool) -> bool {
        let mut screen = self.screen.lock().unwrap();
        let changed = change(&mut screen);
        if changed {
            self.screen_revision.fetch_add(1, Ordering::SeqCst);
        }
        drop(screen);
        if changed {
            self.screen_changes.send_replace(());
        }
        changed
    }

    /// Records how a terminal Tick's program ended, once; the Tick itself keeps running until it
    /// is closed or idles out, so its last screen stays readable.
    fn record_exit(&self, program: &TerminalProgram, exit: ProgramExit) {
        self.change_screen(|_| {
            let mut recorded = program.exit.lock().unwrap();
            let is_first = recorded.is_none();
            if is_first {
                *recorded = Some(exit);
            }
            is_first
        });
    }

    fn terminal_read(&self, program: &TerminalProgram) -> TerminalRead {
        let screen = self.screen.lock().unwrap();
        let snapshot = screen.snapshot();
        TerminalRead {
            seq: self.screen_revision.load(Ordering::SeqCst),
            at: tick_protocol::now_rfc3339(),
            text: snapshot.lines.join("\n"),
            truncated: snapshot.truncated,
            exit: program.exit(),
        }
    }

    fn is_idle(&self, now_ms: u64, idle_timeout: Duration) -> bool {
        matches!(*self.lifecycle.borrow(), Lifecycle::Live { .. })
            && self.views.lock().unwrap().is_empty()
            && self.intents.lock().unwrap().watches.is_empty()
            && now_ms.saturating_sub(self.last_activity_ms.load(Ordering::SeqCst))
                >= idle_timeout.as_millis() as u64
    }

    /// Applies `change` to the attached views, and gives the terminal the size they now lead with.
    fn change_views<T>(&self, change: impl FnOnce(&mut Views) -> T) -> Result<T, HostError> {
        let mut views = self.views.lock().unwrap();
        let changed = change(&mut views);
        self.follow_views(&views)?;
        Ok(changed)
    }

    /// Gives the terminal the size its views lead with. The model follows first, so the redraw
    /// the resize signal causes is parsed at the new size; every view's next frame names it.
    fn follow_views(&self, views: &Views) -> Result<(), HostError> {
        let Some(size) = views.leading_size() else {
            return Ok(());
        };
        let resized = self.change_screen(|screen| {
            let differs = screen.size() != size;
            if differs {
                screen.resize(size);
            }
            differs
        });
        if !resized {
            return Ok(());
        }
        self.pty
            .resize(size)
            .map_err(|error| HostError::after("TerminalUnavailable", error.to_string()))
    }
}

/// Operation receipts, each readable only by the owner it was made for. An expired receipt
/// cannot tell whether its operation ran, so one of an unfinished operation, or of an open whose
/// Tick still runs, never expires; beyond [`RECEIPT_CAPACITY`] the oldest others do.
#[derive(Default)]
struct Receipts {
    entries: VecDeque<(Uuid, TickOwner, Receipt)>,
}

impl Receipts {
    /// Records `receipt`; `running` names the instances still running, asked only when an
    /// older receipt has to expire.
    fn record(
        &mut self,
        operation: Uuid,
        owner: &TickOwner,
        receipt: Receipt,
        running: impl FnOnce() -> HashSet<InstanceRef>,
    ) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(known, recorded, _)| *known == operation && recorded == owner)
        {
            entry.2 = receipt;
            return;
        }
        if self.entries.len() >= RECEIPT_CAPACITY {
            let running = running();
            let mut expiring = self.entries.len() + 1 - RECEIPT_CAPACITY;
            self.entries.retain(|(_, _, kept)| {
                let still_matters = match kept {
                    Receipt::Dispatched => true,
                    Receipt::Opened(opened) => running.contains(&opened.instance),
                    _ => false,
                };
                if expiring == 0 || still_matters {
                    return true;
                }
                expiring -= 1;
                false
            });
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
    /// Shell tasks by operation, each until it was released and nobody viewed it for the idle
    /// timeout.
    shells: HashMap<Uuid, Arc<ShellTask>>,
    spawn_grants: SpawnGrants,
    /// Connected spawners; each keeps the host from exiting, with or without its task yet.
    spawners: usize,
    /// Connections of the current controller generation; once the last one closes, every Shell
    /// task stops.
    controllers: usize,
    receipts: Receipts,
    pending_opens: usize,
    ever_used: bool,
    /// Decided under this lock when the host stays empty; no open starts after it.
    exiting: bool,
    controller_generation: u64,
    controller_epoch: Option<Uuid>,
    /// Per config root: the last applied owner snapshot revision and one being received.
    snapshots: HashMap<String, RootSnapshots>,
    /// Owners ended at a registry revision; their opens at that revision or earlier fail. An
    /// entry goes once a complete snapshot of its root at a later revision is applied.
    ended: HashMap<TickOwner, u64>,
    diagnostics: Diagnostics,
}

impl HostState {
    /// Stops every Shell task: the daemon that granted them is gone or was replaced.
    fn stop_shells(&self) {
        for task in self.shells.values() {
            task.stop(CANCELLED_REASON);
        }
    }

    fn record_receipt(&mut self, operation: Uuid, owner: &TickOwner, receipt: Receipt) {
        let ticks = &self.ticks;
        self.receipts.record(operation, owner, receipt, || {
            ticks
                .values()
                .filter_map(|tick| tick.lifecycle.borrow().live_instance().cloned())
                .collect()
        });
    }

    /// Refuses a controller connection that a newer Agent daemon fenced.
    fn ensure_current_controller(&self, generation: u64) -> Result<(), HostError> {
        if self.controller_generation != generation {
            return Err(HostError::before(
                "Fenced",
                "a newer Agent daemon controls this Tickhost",
            ));
        }
        Ok(())
    }
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

/// What the daemon starts a host with.
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
    /// Names each Tick's session directory.
    session_serial: AtomicU64,
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

    async fn receipt(&self, operation: Uuid, owner: &TickOwner) -> Option<Receipt> {
        self.state.lock().await.receipts.get(operation, owner)
    }

    async fn tick_by_instance(&self, instance: &InstanceRef) -> Option<Arc<HostedTick>> {
        let state = self.state.lock().await;
        state
            .ticks
            .values()
            .find(|tick| tick.lifecycle.borrow().live_instance() == Some(instance))
            .cloned()
    }

    fn terminal_binding(&self, terminal: Uuid) -> HostedTerminal {
        HostedTerminal {
            endpoint: self.paths.socket(),
            epoch: self.epoch,
            terminal,
        }
    }

    async fn open(
        self: &Arc<Self>,
        request: OpenRequest,
        generation: u64,
    ) -> Result<OpenedTick, HostError> {
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
                    "the Tickhost is stopping",
                ));
            }
            validate_launch(&request)?;
            state.pending_opens += 1;
            state.ever_used = true;
            state.record_receipt(request.operation, &request.owner, Receipt::Dispatched);
        }
        let result = self.launch(&request, generation).await;
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
        self.state
            .lock()
            .await
            .record_receipt(request.operation, &request.owner, receipt);
        result
    }

    /// Starts `request` on a private terminal. A terminal program is Live as soon as it runs; a
    /// protocol Tick only once its launcher registered it, within the control timeout.
    async fn launch(
        self: &Arc<Self>,
        request: &OpenRequest,
        generation: u64,
    ) -> Result<OpenedTick, HostError> {
        let terminal = Uuid::new_v4();
        let size = TerminalSize::default();
        let (pty, slave) = Pty::open(size)
            .map_err(|error| HostError::before("TerminalUnavailable", error.to_string()))?;
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
            .env("TERM", "xterm-256color");
        let adapter = match request.adapter {
            TickAdapter::Protocol => {
                self.paths
                    .check_session_directories()
                    .map_err(|error| HostError::before("LaunchFailed", error.to_string()))?;
                let session_directory = self
                    .paths
                    .tick_session_directory(self.session_serial.fetch_add(1, Ordering::Relaxed));
                let launch_token = random_token();
                let binding = LaunchBinding {
                    terminal: self.terminal_binding(terminal),
                    operation: request.operation,
                    launch_token: launch_token.clone(),
                };
                command
                    .env(
                        hosted::LAUNCH_ENV,
                        serde_json::to_string(&binding).expect("launch binding serializes"),
                    )
                    .env(
                        tickhost_connect::session::RUNTIME_DIR_ENV,
                        &session_directory,
                    );
                Adapter::Protocol(ProtocolLaunch {
                    launch_token,
                    launcher_claimed: AtomicBool::new(false),
                    session_directory,
                    session: Mutex::new(None),
                })
            }
            TickAdapter::Terminal => Adapter::Terminal(TerminalProgram {
                command: request.command.clone(),
                cwd: request.cwd.clone(),
                exit: std::sync::Mutex::new(None),
            }),
        };
        slave
            .attach(&mut command)
            .map_err(|error| HostError::before("TerminalUnavailable", error.to_string()))?;
        // The registry guard covers the controller and owner fences, the spawn and the publication
        // of the launch token: a newer controller or an owner end either comes first and refuses
        // this launch, or comes after the Tick is registered and finds it with the rest.
        let mut state = self.state.lock().await;
        state.ensure_current_controller(generation)?;
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
        let launcher = command
            .spawn()
            .map_err(|error| HostError::before("LaunchFailed", error.to_string()))?;
        let launcher_pid = launcher.id().ok_or_else(|| {
            HostError::before("LaunchFailed", "the launcher exited before it started")
        })?;
        let lifecycle = match adapter {
            Adapter::Protocol(_) => Lifecycle::Opening,
            Adapter::Terminal(_) => Lifecycle::Live {
                instance: InstanceRef::hosted(Uuid::new_v4()),
                business_pid: launcher_pid,
            },
        };
        let tick = Arc::new(HostedTick {
            terminal,
            operation: request.operation,
            owner: request.owner.clone(),
            title: request.title.as_deref().map(display_title),
            adapter,
            data_token: random_token(),
            pty,
            screen: std::sync::Mutex::new(Screen::new(size)),
            screen_revision: AtomicU64::new(0),
            launcher: Mutex::new(launcher),
            launcher_pid,
            lifecycle: watch::channel(lifecycle).0,
            last_activity_ms: AtomicU64::new(Self::now_ms()),
            views: std::sync::Mutex::new(Views::default()),
            view_grants: std::sync::Mutex::new(ViewGrants::default()),
            intents: std::sync::Mutex::new(WatchIntents::default()),
            screen_changes: watch::channel(()).0,
            terminal_closed: CancellationToken::new(),
        });
        state.ticks.insert(terminal, tick.clone());
        drop(state);
        tokio::spawn(drain_terminal(tick.clone()));
        let opened = |instance, business_pid| OpenedTick {
            instance,
            adapter: tick.adapter.kind(),
            terminal: self.terminal_binding(terminal),
            business_pid,
            data_token: tick.data_token.clone(),
        };
        if let Lifecycle::Live {
            instance,
            business_pid,
        } = tick.lifecycle.borrow().clone()
        {
            return Ok(opened(instance, business_pid));
        }
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
            Ok(Some((instance, business_pid))) => Ok(opened(instance, business_pid)),
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
        self.state
            .lock()
            .await
            .record_receipt(operation, owner, Receipt::Dispatched);
        if let Some(tick) = tick {
            self.terminate(&tick).await;
            self.reap(tick.terminal, ReapReason::Closed).await;
        }
        let receipt = Receipt::Closed { instance };
        self.state
            .lock()
            .await
            .record_receipt(operation, owner, receipt.clone());
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
            // An owner ended before this revision needs no fence any more: an older controller's
            // open is refused at spawn, and a daemon serves a root only after reconciling it, so
            // its opens carry this revision or a later one.
            state
                .ended
                .retain(|owner, ended| owner.root != root || *ended >= revision);
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
            state.record_receipt(operation, &owner, Receipt::Dispatched);
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
        self.state
            .lock()
            .await
            .record_receipt(operation, &owner, receipt.clone());
        Ok(receipt)
    }

    /// Ends `ticks` side by side, so one that ignores SIGTERM delays none of the others.
    async fn end_ticks(&self, ticks: Vec<Arc<HostedTick>>, reason: ReapReason) -> Vec<InstanceRef> {
        let mut ended = Vec::new();
        let mut stopping = tokio::task::JoinSet::new();
        for tick in &ticks {
            ended.extend(
                tick.lifecycle
                    .send_replace(Lifecycle::Closing)
                    .live_instance()
                    .cloned(),
            );
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
        let instance = tick.lifecycle.borrow().live_instance().cloned();
        state.diagnostics.note_reaped(ReapedTick {
            terminal,
            instance,
            reason,
        });
        drop(state);
        tick.terminal_closed.cancel();
        tick.remove_session_directory();
    }

    /// Lets one more view attach to `owner`'s Tick `instance`; earlier grants stay usable.
    async fn allow_view(
        &self,
        owner: &TickOwner,
        instance: &InstanceRef,
    ) -> Result<Value, HostError> {
        let tick = self
            .tick_by_instance(instance)
            .await
            .filter(|tick| &tick.owner == owner)
            .ok_or_else(|| HostError::before("TargetGone", "no such Tick of this owner"))?;
        let view = Uuid::new_v4();
        tick.view_grants.lock().unwrap().allow(view, Instant::now());
        Ok(json!({"instance":instance,"view":view,"epoch":self.epoch}))
    }

    /// Lets one more view attach to the Shell task `instance` that `identity` submitted, running
    /// or ended; earlier grants stay usable.
    async fn allow_shell_view(
        &self,
        identity: &ShellIdentity,
        instance: &InstanceRef,
    ) -> Result<Value, HostError> {
        let task = self
            .state
            .lock()
            .await
            .shells
            .values()
            .find(|task| &task.instance == instance && &task.binding.identity == identity)
            .cloned()
            .ok_or_else(|| HostError::before("TargetGone", "no such Shell task of this Chat"))?;
        let view = Uuid::new_v4();
        task.view_grants.lock().unwrap().allow(view, Instant::now());
        Ok(json!({"instance":instance,"view":view,"epoch":self.epoch}))
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

    /// Grants one spawn of what `binding` names, for its spawner to claim within the grant
    /// timeout. Only the current controller's own epoch can be bound.
    async fn allow_spawn(&self, binding: SpawnBinding) -> Result<Value, HostError> {
        let mut state = self.state.lock().await;
        if self.shutdown.is_cancelled() || state.exiting {
            return Err(HostError::before(
                "ShuttingDown",
                "the Tickhost is stopping",
            ));
        }
        if state.controller_epoch != Some(binding.identity.daemon_epoch) {
            return Err(HostError::before(
                "Fenced",
                "a spawn is granted only for the current Agent daemon",
            ));
        }
        state.ever_used = true;
        let token = random_token();
        let grant = state
            .spawn_grants
            .issue(binding, token.clone(), Instant::now());
        Ok(json!(SpawnGrant {
            grant,
            token,
            epoch: self.epoch,
        }))
    }

    /// Starts the task a claimed grant admitted. The request must be exactly what the grant
    /// binds, for the daemon that still controls this host.
    async fn start_shell(
        &self,
        granted: &SpawnBinding,
        request: SpawnRequest,
        directory: std::os::fd::OwnedFd,
        output: std::os::fd::OwnedFd,
    ) -> Result<Arc<ShellTask>, HostError> {
        if &request.binding != granted {
            return Err(HostError::before(
                "Forbidden",
                "the request differs from what its spawn grant binds",
            ));
        }
        let mut state = self.state.lock().await;
        if self.shutdown.is_cancelled() || state.exiting {
            return Err(HostError::before(
                "ShuttingDown",
                "the Tickhost is stopping",
            ));
        }
        if state.controller_epoch != Some(granted.identity.daemon_epoch) {
            return Err(HostError::before(
                "Fenced",
                "the Agent daemon that granted this spawn no longer controls this Tickhost",
            ));
        }
        let operation = granted.operation;
        if state.shells.contains_key(&operation) {
            return Err(HostError::before(
                "Forbidden",
                "this operation already runs a Shell task",
            ));
        }
        let task = ShellTask::start(request, directory, output, random_token())?;
        state.shells.insert(operation, task.clone());
        Ok(task)
    }

    /// Releases a task once its spawner left: a task still running is stopped first.
    async fn release_shell(&self, task: &Arc<ShellTask>) {
        task.stop(CANCELLED_REASON);
        task.ended().await;
        task.release();
    }

    /// Reaps Ticks whose launcher exited, starts ending those that stayed idle, forgets released
    /// Shell tasks nobody viewed for the idle timeout, and reports whether the host should exit
    /// as empty. Idle Ticks stop in the background, so one that ignores SIGTERM never pauses
    /// accepting connections; until reaped it keeps the host from exiting.
    async fn sweep(self: &Arc<Self>) -> bool {
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
            // A closing Tick is reaped by whatever is closing it, under its own reason.
            if matches!(*tick.lifecycle.borrow(), Lifecycle::Closing) {
                continue;
            }
            // A terminal Tick outlives its program: its drain records the exit.
            let launcher_exited = matches!(tick.adapter, Adapter::Protocol(_))
                && tick
                    .launcher
                    .try_lock()
                    .is_ok_and(|mut launcher| !matches!(launcher.try_wait(), Ok(None)));
            if launcher_exited {
                self.reap(tick.terminal, ReapReason::Exited).await;
                continue;
            }
            tick.lapse_stale_intents(Self::now_ms(), self.idle_timeout);
            if tick.is_idle(Self::now_ms(), self.idle_timeout) {
                // Closing at once, so no later sweep picks it again while it stops.
                tick.lifecycle.send_replace(Lifecycle::Closing);
                idle.push(tick);
            }
        }
        if !idle.is_empty() {
            let host = Arc::clone(self);
            tokio::spawn(async move { host.end_ticks(idle, ReapReason::Idle).await });
        }
        let mut state = self.state.lock().await;
        let now_ms = Self::now_ms();
        state
            .shells
            .retain(|_, task| !task.is_forgotten(now_ms, self.idle_timeout));
        let has_pending_spawns = state.spawn_grants.has_pending(Instant::now());
        state.exiting = state.ticks.is_empty()
            && state.shells.is_empty()
            && state.spawners == 0
            && !has_pending_spawns
            && state.pending_opens == 0
            && (state.ever_used || self.started.elapsed() >= INITIAL_GRACE);
        state.exiting
    }

    /// One page of the host's Ticks, optionally only those of one owner.
    async fn list(
        &self,
        owner: Option<&TickOwner>,
        cursor: Option<tickhost_connect::page::PageCursor>,
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
                let exit = match &tick.adapter {
                    Adapter::Protocol(_) => None,
                    Adapter::Terminal(program) => program.exit(),
                };
                TickRow {
                    instance: lifecycle.live_instance().cloned(),
                    adapter: tick.adapter.kind(),
                    terminal: tick.terminal,
                    data_token: matches!(lifecycle, Lifecycle::Live { .. })
                        .then(|| tick.data_token.clone()),
                    owner: tick.owner.clone(),
                    title: tick.title.clone(),
                    state: match lifecycle {
                        Lifecycle::Opening => TickState::Opening,
                        Lifecycle::Live { .. } if exit.is_some() => TickState::Exited,
                        Lifecycle::Live { .. } => TickState::Running,
                        Lifecycle::Closing => TickState::Closing,
                    },
                    business_pid: match lifecycle {
                        Lifecycle::Live { business_pid, .. } => Some(business_pid),
                        _ => None,
                    },
                    views: tick.views.lock().unwrap().len(),
                    exit,
                }
            })
            .collect::<Vec<_>>();
        let page = tickhost_connect::page::paginate(
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
            "shells": state.shells.len(),
            "pending_opens": state.pending_opens,
            "errors": state.diagnostics.errors,
            "last_error": state.diagnostics.last_error,
            "reaped": state.diagnostics.reaped,
        })
    }

    async fn shut_down(&self) {
        let (ticks, shells) = {
            let state = self.state.lock().await;
            (
                state.ticks.values().cloned().collect::<Vec<_>>(),
                state.shells.values().cloned().collect::<Vec<_>>(),
            )
        };
        let mut closing = tokio::task::JoinSet::new();
        for tick in ticks {
            closing.spawn(async move {
                stop_launcher(&tick).await;
                tick.terminal_closed.cancel();
                tick.remove_session_directory();
            });
        }
        for task in shells {
            closing.spawn(async move {
                task.stop(CANCELLED_REASON);
                task.ended().await;
                task.released.cancel();
            });
        }
        while closing.join_next().await.is_some() {}
        let mut state = self.state.lock().await;
        state.ticks.clear();
        state.shells.clear();
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
    let group = tick.launcher_pid as libc::pid_t;
    if let Adapter::Terminal(_) = tick.adapter {
        // The program is waited for only here, so even after it exited its group id is still its
        // own: whatever it left running in that group ends with the Tick.
        let Ok(exited) = process::peek_exit(tick.launcher_pid) else {
            return;
        };
        unsafe { libc::killpg(group, libc::SIGTERM) };
        if exited.is_none() {
            let _ = tokio::time::timeout(TERMINATE_GRACE, async {
                while matches!(process::peek_exit(tick.launcher_pid), Ok(None)) {
                    tokio::time::sleep(EXIT_POLL).await;
                }
            })
            .await;
        }
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let _ = launcher.wait().await;
        return;
    }
    if !matches!(launcher.try_wait(), Ok(None)) {
        return;
    }
    unsafe { libc::killpg(group, libc::SIGTERM) };
    if tokio::time::timeout(TERMINATE_GRACE, launcher.wait())
        .await
        .is_err()
    {
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let _ = launcher.wait().await;
    }
}

/// Drains the PTY continuously so a hidden Tick never blocks on output. A protocol Tick ends
/// with its terminal. A terminal Tick keeps its last screen: once everything its program wrote
/// was parsed and the program exited, the exit is recorded with that screen.
async fn drain_terminal(tick: Arc<HostedTick>) {
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    loop {
        tokio::select! {
            _ = tick.terminal_closed.cancelled() => return,
            read = tick.pty.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    tick.change_screen(|screen| {
                        screen.process(&buffer[..count]);
                        true
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
    }
    match &tick.adapter {
        Adapter::Protocol(_) => tick.terminal_closed.cancel(),
        Adapter::Terminal(program) => loop {
            match process::peek_exit(tick.launcher_pid) {
                Ok(Some(exit)) => return tick.record_exit(program, exit),
                // Only stopping the Tick waits for its program.
                Err(_) => return,
                Ok(None) => {}
            }
            tokio::select! {
                _ = tick.terminal_closed.cancelled() => return,
                _ = tokio::time::sleep(EXIT_POLL) => {}
            }
        },
    }
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

/// `title` as it is kept and listed: at most [`TITLE_CHARACTERS`] characters, the last one
/// [`TITLE_ELLIPSIS`] when anything was cut.
pub fn display_title(title: &str) -> String {
    if title.chars().nth(TITLE_CHARACTERS).is_none() {
        return title.to_owned();
    }
    let mut shown = title.chars().take(TITLE_CHARACTERS - 1).collect::<String>();
    shown.push(TITLE_ELLIPSIS);
    shown
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
    serve_until(paths, config, std::future::pending()).await
}

/// [`serve`], also ending once `stop` completes, as `tickhost run` does on SIGTERM.
pub async fn serve_until(
    paths: HostPaths,
    config: HostConfig,
    stop: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
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
            "another Tickhost owns this runtime directory",
        ));
    }
    // `tickhost stop` signals the process the lock names.
    lock.set_len(0)?;
    (&lock).write_all(std::process::id().to_string().as_bytes())?;
    let socket = paths.socket();
    if std::fs::symlink_metadata(&socket).is_ok() {
        // The lock proves no live host serves this path; only a stale socket of ours remains.
        let metadata = std::fs::symlink_metadata(&socket)?;
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if !metadata.file_type().is_socket() || metadata.uid() != paths::current_uid() {
            return Err(std::io::Error::other(
                "the Tickhost endpoint path is occupied by something else",
            ));
        }
        std::fs::remove_file(&socket)?;
    }
    paths.prune_emptied(EMPTIED_MARK_RETENTION);
    paths.clear_sessions();
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let identity = paths::file_identity(&socket)?;
    let host = Arc::new(Host {
        paths,
        epoch: Uuid::new_v4(),
        controller_key: config.controller_key,
        idle_timeout: config.idle_timeout,
        started: Instant::now(),
        session_serial: AtomicU64::new(0),
        state: Mutex::new(HostState {
            ticks: HashMap::new(),
            shells: HashMap::new(),
            spawn_grants: SpawnGrants::default(),
            spawners: 0,
            controllers: 0,
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
    tokio::pin!(stop);
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
            _ = &mut stop => break,
        }
    }
    host.shutdown.cancel();
    host.shutdown_state.send_replace(ShutdownState::Stopping);
    host.shut_down().await;
    if emptied {
        // Without the mark a daemon later counts this host's Ticks as lost rather than ended.
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
            let _ = write_within(
                &mut writer,
                &hosted::error(&Value::Null, &error),
                MANAGEMENT_FRAME_BYTES,
                CONTROL_TIMEOUT,
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
                    "management requires the frozen 1.0 protocol and the daemon's controller key",
                );
                let _ = write_within(
                    &mut writer,
                    &hosted::error(&Value::Null, &error),
                    MANAGEMENT_FRAME_BYTES,
                    CONTROL_TIMEOUT,
                )
                .await;
                return Err(error);
            }
            write_within(
                &mut writer,
                &hosted::ok(
                    &Value::Null,
                    json!(HelloReply {
                        epoch: host.epoch,
                        pid: std::process::id(),
                        version: BUILD_VERSION.to_owned(),
                        major: MANAGEMENT_MAJOR,
                        minor: MANAGEMENT_MINOR,
                        capabilities: Vec::new(),
                    }),
                ),
                MANAGEMENT_FRAME_BYTES,
                CONTROL_TIMEOUT,
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
                    format!("Tickhost serves business protocol {BUSINESS_MAJOR}.{BUSINESS_MINOR}"),
                ))
            } else {
                required
                    .iter()
                    .find(|capability| !BUSINESS_CAPABILITIES.contains(&capability.as_str()))
                    .map(|missing| {
                        HostError::before(
                            "IncompatibleHost",
                            format!("Tickhost lacks the {missing} capability"),
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
                    let _ = write_within(
                        &mut writer,
                        &hosted::error(&Value::Null, &error),
                        BUSINESS_FRAME_BYTES,
                        CONTROL_TIMEOUT,
                    )
                    .await;
                    return Err(error);
                }
            };
            let departure = Departure::of(&session);
            let welcomed = write_within(
                &mut writer,
                &hosted::ok(
                    &Value::Null,
                    json!(HelloReply {
                        epoch: host.epoch,
                        pid: std::process::id(),
                        version: BUILD_VERSION.to_owned(),
                        major: BUSINESS_MAJOR,
                        minor: BUSINESS_MINOR,
                        capabilities: BUSINESS_CAPABILITIES
                            .iter()
                            .map(|value| (*value).to_owned())
                            .collect(),
                    }),
                ),
                BUSINESS_FRAME_BYTES,
                CONTROL_TIMEOUT,
            )
            .await;
            let result = match welcomed {
                Ok(()) => business(reader, writer, host.clone(), session).await,
                Err(error) => Err(error),
            };
            departure.leave(&host).await;
            result
        }
    }
}

/// What an authenticated connection holds of the host until it ends.
enum Departure {
    /// A connection of the controller generation it names, one of the daemon's while it is current.
    Controller(u64),
    /// A spawner, which keeps the host from exiting.
    Spawner,
    Other,
}

impl Departure {
    fn of(authority: &Authority) -> Self {
        match authority {
            Authority::Controller { generation } => Self::Controller(*generation),
            Authority::Spawner(_) => Self::Spawner,
            _ => Self::Other,
        }
    }

    /// Accounts for the connection's end: the last connection of the current controller stops
    /// every Shell task.
    async fn leave(self, host: &Host) {
        let mut state = host.state.lock().await;
        match self {
            Self::Controller(generation) if state.controller_generation == generation => {
                state.controllers -= 1;
                if state.controllers == 0 {
                    state.stop_shells();
                }
            }
            Self::Spawner => state.spawners -= 1,
            Self::Controller(_) | Self::Other => {}
        }
    }
}

enum Authority {
    Controller {
        generation: u64,
    },
    Instance(Arc<HostedTick>),
    /// A reader of one Shell task's `shell:` instance.
    Shell(Arc<ShellTask>),
    /// The one connection a spawn grant admitted; the task it submits lives as long as it.
    Spawner(SpawnBinding),
    /// A view a grant let in; it can only attach.
    Viewer(Viewed),
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
            daemon_epoch,
        } => {
            if !same_secret(&controller_key, &host.controller_key) {
                return Err(HostError::before(
                    "Forbidden",
                    "only the Agent daemon controls Tick lifecycles",
                ));
            }
            let mut state = host.state.lock().await;
            if state.controller_epoch != Some(daemon_epoch) {
                // A new daemon fences every earlier controller connection, and the Shell tasks
                // and spawn grants of the daemon it replaced end with them.
                state.controller_generation += 1;
                state.controller_epoch = Some(daemon_epoch);
                state.controllers = 0;
                state.spawn_grants.clear();
                state.stop_shells();
            }
            state.controllers += 1;
            Ok(Authority::Controller {
                generation: state.controller_generation,
            })
        }
        Role::Instance {
            instance,
            data_token,
        } if instance.tick_type() == tick_protocol::SHELL_INSTANCE_TYPE => host
            .state
            .lock()
            .await
            .shells
            .values()
            .find(|task| {
                task.instance == instance
                    && !task.released.is_cancelled()
                    && same_secret(&data_token, &task.data_token)
            })
            .cloned()
            .map(Authority::Shell)
            .ok_or_else(|| HostError::before("Forbidden", "unknown instance or data token")),
        Role::Instance {
            instance,
            data_token,
        } => host
            .tick_by_instance(&instance)
            .await
            .filter(|tick| same_secret(&data_token, &tick.data_token))
            .map(Authority::Instance)
            .ok_or_else(|| HostError::before("Forbidden", "unknown instance or data token")),
        Role::Viewer { instance, view } => {
            let viewed = if instance.tick_type() == tick_protocol::SHELL_INSTANCE_TYPE {
                host.state
                    .lock()
                    .await
                    .shells
                    .values()
                    .find(|task| task.instance == instance)
                    .filter(|task| task.view_grants.lock().unwrap().claim(view, Instant::now()))
                    .cloned()
                    .map(Viewed::Shell)
            } else {
                host.tick_by_instance(&instance)
                    .await
                    .filter(|tick| tick.view_grants.lock().unwrap().claim(view, Instant::now()))
                    .map(Viewed::Tick)
            };
            viewed.map(Authority::Viewer).ok_or_else(|| {
                HostError::before("Forbidden", "unknown, expired or used view grant")
            })
        }
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
                    && matches!(&tick.adapter, Adapter::Protocol(launch) if same_secret(&launch_token, &launch.launch_token))
            })
            .cloned()
            .filter(|tick| {
                matches!(&tick.adapter, Adapter::Protocol(launch) if !launch.launcher_claimed.swap(true, Ordering::SeqCst))
            })
            .map(|tick| Authority::Launcher {
                tick,
                verified: false,
            })
            .ok_or_else(|| HostError::before("Forbidden", "unknown or already used launch token")),
        Role::Spawner { grant, token } => {
            let mut state = host.state.lock().await;
            let binding = state
                .spawn_grants
                .claim(grant, &token, Instant::now())
                .ok_or_else(|| {
                    HostError::before("Forbidden", "unknown, expired or used spawn grant")
                })?;
            state.spawners += 1;
            Ok(Authority::Spawner(binding))
        }
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
        write_within(&mut writer, &frame, MANAGEMENT_FRAME_BYTES, CONTROL_TIMEOUT).await?;
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
            "this endpoint now serves another Tickhost process",
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

/// Serves one authenticated connection; a spawner's task ends with its connection.
async fn business(
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    host: Arc<Host>,
    authority: Authority,
) -> Result<(), HostError> {
    match authority {
        Authority::Spawner(binding) => serve_spawner(reader, writer, &host, &binding).await,
        authority => serve_requests(reader, writer, host, authority).await,
    }
}

async fn serve_requests(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: Arc<Host>,
    mut authority: Authority,
) -> Result<(), HostError> {
    // A connection bound to one Tick closes when that Tick ends, so an idle one never keeps the
    // reaped Tick's terminal; a controller's token never ends.
    let tick_ended = match &authority {
        // A Shell's view keeps showing the task after it ended.
        Authority::Controller { .. }
        | Authority::Spawner(_)
        | Authority::Viewer(Viewed::Shell(_)) => CancellationToken::new(),
        Authority::Instance(tick)
        | Authority::Viewer(Viewed::Tick(tick))
        | Authority::Launcher { tick, .. } => tick.terminal_closed.clone(),
        Authority::Shell(task) => task.released.clone(),
    };
    loop {
        let request = tokio::select! {
            biased;
            _ = host.shutdown.cancelled() => return Ok(()),
            _ = tick_ended.cancelled() => return Ok(()),
            request = hosted::read_frame(&mut reader, BUSINESS_FRAME_BYTES) => request,
        };
        let request = match request {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(error) => {
                let _ = write_within(
                    &mut writer,
                    &hosted::error(&Value::Null, &error),
                    BUSINESS_FRAME_BYTES,
                    CONTROL_TIMEOUT,
                )
                .await;
                return Err(error);
            }
        };
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        if let Authority::Shell(task) = &authority
            && method == "watch"
        {
            return watch_shell(reader, writer, &host, task.clone(), &id).await;
        }
        if let Authority::Instance(tick) = &authority
            && method == "watch"
        {
            return match tick.adapter {
                Adapter::Protocol(_) => {
                    stream_observations(reader, writer, &host, tick.clone(), &id).await
                }
                Adapter::Terminal(_) => {
                    watch_terminal(reader, writer, &host, tick.clone(), &id).await
                }
            };
        }
        let viewed = match &authority {
            Authority::Instance(tick) => Some(Viewed::Tick(tick.clone())),
            Authority::Viewer(viewed) => Some(viewed.clone()),
            _ => None,
        };
        if let Some(viewed) = viewed
            && method == "attach"
        {
            match ViewLease::acquire(viewed, params) {
                Ok(lease) => return attach_view(reader, writer, &host, lease, &id).await,
                Err(error) => {
                    write_within(
                        &mut writer,
                        &hosted::error(&id, &error),
                        BUSINESS_FRAME_BYTES,
                        CONTROL_TIMEOUT,
                    )
                    .await?;
                    continue;
                }
            }
        }
        let reply = match &mut authority {
            Authority::Controller { generation } => {
                controller_request(&host, *generation, &method, params).await
            }
            Authority::Instance(tick) => instance_request(tick, &method, params).await,
            Authority::Shell(task) => shell_request(task, &method),
            Authority::Spawner(_) => unreachable!("a spawner is served on its own"),
            Authority::Viewer(_) => Err(HostError::before(
                "MethodNotAllowed",
                format!("a view can only attach, not {method:?}"),
            )),
            Authority::Launcher { tick, verified } => {
                launcher_request(&host, tick, verified, &method, params).await
            }
        };
        let frame = match reply {
            Ok(result) => hosted::ok(&id, result),
            Err(error) => hosted::error(&id, &error),
        };
        write_within(&mut writer, &frame, BUSINESS_FRAME_BYTES, CONTROL_TIMEOUT).await?;
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
    host.state
        .lock()
        .await
        .ensure_current_controller(generation)?;
    match method {
        "open" => host
            .open(parameters(params)?, generation)
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
                cursor: Option<tickhost_connect::page::PageCursor>,
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
        "allow_view" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Allow {
                owner: TickOwner,
                instance: InstanceRef,
            }
            let Allow { owner, instance } = parameters(params)?;
            host.allow_view(&owner, &instance).await
        }
        "allow_shell_view" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Allow {
                identity: ShellIdentity,
                instance: InstanceRef,
            }
            let Allow { identity, instance } = parameters(params)?;
            host.allow_shell_view(&identity, &instance).await
        }
        "reconcile_owners" => host.reconcile_owners(parameters(params)?).await,
        "allow_spawn" => host.allow_spawn(parameters(params)?).await,
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
    let Adapter::Protocol(launch) = &tick.adapter else {
        return Err(HostError::before(
            "UnsupportedCapability",
            "a terminal Tick does not speak the Tick protocol",
        ));
    };
    launch
        .session
        .lock()
        .await
        .clone()
        .ok_or_else(|| HostError::before("TargetGone", "the Tick is not running"))
}

fn session_error(error: tickhost_connect::session::SessionError) -> HostError {
    use tickhost_connect::session::SessionError;
    match error {
        SessionError::Busy => HostError::before("Busy", error.to_string()),
        SessionError::CancelledBeforeSend => HostError::before("Cancelled", error.to_string()),
        SessionError::OutcomeUnknown => HostError::after("OutcomeUnknown", error.to_string()),
        SessionError::Gone => HostError::after("TargetGone", error.to_string()),
        SessionError::Refused(_) => HostError::before("Refused", error.to_string()),
        SessionError::ToolFailed(message) => {
            HostError::after(tickhost_connect::host_error::TOOL_FAILED, message)
        }
        SessionError::RevisionMismatch => {
            HostError::before("CapabilitiesChanged", error.to_string())
        }
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
            match &tick.adapter {
                Adapter::Protocol(_) => session(tick)
                    .await?
                    .read()
                    .await
                    .map(|read| json!(read))
                    .map_err(session_error),
                Adapter::Terminal(program) => Ok(json!(tick.terminal_read(program))),
            }
        }
        "resources" => {
            let Lifecycle::Live { business_pid, .. } = tick.lifecycle.borrow().clone() else {
                return Err(HostError::before("TargetGone", "the Tick is not running"));
            };
            if let Adapter::Terminal(program) = &tick.adapter
                && program.exit().is_some()
            {
                return Err(HostError::before(
                    "ProgramExited",
                    "the program exited; it has no resources to sample",
                ));
            }
            let sample = process::sample(business_pid);
            Ok(json!(Resources {
                business_pid,
                cpu_time_ms: sample.and_then(|sample| sample.cpu_time_ms),
                rss_bytes: sample.and_then(|sample| sample.rss_bytes),
                sampled_at_ms: Host::now_ms(),
            }))
        }
        _ => match &tick.adapter {
            Adapter::Protocol(_) => protocol_request(tick, method, params).await,
            Adapter::Terminal(program) => terminal_request(tick, program, method, params).await,
        },
    }
}

async fn protocol_request(
    tick: &Arc<HostedTick>,
    method: &str,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        "describe" => session(tick)
            .await?
            .describe()
            .await
            .map(|descriptor| json!(descriptor))
            .map_err(session_error),
        "call" => {
            #[derive(Deserialize)]
            struct Call {
                tool: String,
                args: Value,
                revision: String,
            }
            let Call {
                tool,
                args,
                revision,
            } = parameters(params)?;
            tick.note_activity();
            let client = session(tick).await?;
            tokio::time::timeout(
                CALL_TIMEOUT,
                client.call(&tool, args, &revision, CancellationToken::new()),
            )
            .await
            .map_err(|_| {
                HostError::after("OutcomeUnknown", "the Tick Tool did not finish in time")
            })?
            .map_err(session_error)
        }
        // When the Tick last updated; reading this is not using the Tick and reads no content.
        "status" => session(tick)
            .await?
            .status()
            .await
            .map(|status| json!(status))
            .map_err(session_error),
        "input" => Err(HostError::before(
            "UnsupportedCapability",
            "a protocol Tick is operated through its declared Tick Tools",
        )),
        _ => Err(HostError::before(
            "MethodNotAllowed",
            format!("an instance reader has no {method:?} method"),
        )),
    }
}

/// The terminal adapter: a terminal Tick describes what it runs and is called with input for
/// its program. Its updates are reads its subscriptions take, so it has no status of its own.
async fn terminal_request(
    tick: &Arc<HostedTick>,
    program: &TerminalProgram,
    method: &str,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        "describe" => Ok(json!(TerminalDescription {
            title: tick.title.clone(),
            command: program.command.clone(),
            cwd: program.cwd.clone(),
        })),
        "input" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                data: Vec<u8>,
            }
            let Input { data } = parameters(params)?;
            if data.is_empty() || data.len() > ATTACH_INPUT_BYTES {
                return Err(HostError::before(
                    "InvalidArguments",
                    format!("input is 1 to {ATTACH_INPUT_BYTES} bytes"),
                ));
            }
            if program.exit().is_some() {
                return Err(HostError::before(
                    "ProgramExited",
                    "the program exited; its last screen stays readable until the Tick is closed",
                ));
            }
            tick.note_activity();
            tokio::time::timeout(INPUT_WRITE_TIMEOUT, tick.pty.write_all(&data))
                .await
                .map_err(|_| {
                    HostError::after(
                        "OutcomeUnknown",
                        "the program did not take its input in time; part of it may have arrived",
                    )
                })?
                .map_err(|error| HostError::after("TerminalUnavailable", error.to_string()))?;
            Ok(json!({"written":data.len()}))
        }
        "call" => Err(HostError::before(
            "UnsupportedCapability",
            "a terminal Tick declares no Tick Tools; send its program input instead",
        )),
        "status" => Err(HostError::before(
            "UnsupportedCapability",
            "a terminal Tick updates only as its subscriptions read its screen",
        )),
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
            let Adapter::Protocol(launch) = &tick.adapter else {
                return Err(HostError::before(
                    "Forbidden",
                    "only a protocol Tick has a launcher",
                ));
            };
            let client = SessionClient::connect(
                instance.clone(),
                host.terminal_binding(tick.terminal),
                Some(launch.session_directory.clone()),
            )
            .await
            .map_err(session_error)?;
            client.describe().await.map_err(session_error)?;
            *launch.session.lock().await = Some(client);
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
    write_within(
        &mut writer,
        &hosted::ok(id, json!({"read":read,"received":received})),
        BUSINESS_FRAME_BYTES,
        CONTROL_TIMEOUT,
    )
    .await?;
    loop {
        tokio::select! {
            changed = observations.changed() => if changed.is_err() { return Ok(()) },
            _ = reader.fill_buf() => return Ok(()),
            _ = tick.terminal_closed.cancelled() => {
                let _ = write_within(&mut writer, &json!({"event":"gone"}), BUSINESS_FRAME_BYTES, STREAM_WRITE_TIMEOUT).await;
                return Ok(());
            }
            _ = host.shutdown.cancelled() => return Ok(()),
        }
        let observation = observations.borrow_and_update().clone();
        write_within(
            &mut writer,
            &json!({"event":"observation","observation":observation}),
            BUSINESS_FRAME_BYTES,
            STREAM_WRITE_TIMEOUT,
        )
        .await?;
    }
}

/// Registers a watch of a terminal Tick: the reply carries its screen now, and the stream then
/// only tells when the Tick ends. Nothing is pushed in between; each subscription reads the
/// screen again at its own frequency. The reader's EOF, or anything it sends, ends the watch.
async fn watch_terminal(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: &Arc<Host>,
    tick: Arc<HostedTick>,
    id: &Value,
) -> Result<(), HostError> {
    use tokio::io::AsyncBufReadExt;
    let Adapter::Terminal(program) = &tick.adapter else {
        return Err(HostError::before(
            "UnsupportedCapability",
            "only a terminal Tick is watched without the Tick protocol",
        ));
    };
    write_within(
        &mut writer,
        &hosted::ok(id, json!({"read":tick.terminal_read(program)})),
        BUSINESS_FRAME_BYTES,
        CONTROL_TIMEOUT,
    )
    .await?;
    tokio::select! {
        _ = reader.fill_buf() => Ok(()),
        _ = tick.terminal_closed.cancelled() => {
            let _ = write_within(&mut writer, &json!({"event":"gone"}), BUSINESS_FRAME_BYTES, STREAM_WRITE_TIMEOUT).await;
            Ok(())
        }
        _ = host.shutdown.cancelled() => Ok(()),
    }
}

/// Serves one spawner: the descriptors and request its grant admits, then the task's exit, once,
/// and its stop requests, until the spawner leaves, which stops a task still running.
async fn serve_spawner(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: &Arc<Host>,
    granted: &SpawnBinding,
) -> Result<(), HostError> {
    let submitted = async {
        let descriptors = hosted::receive_descriptors(&reader, SPAWN_DESCRIPTORS).await?;
        let request = hosted::read_frame(&mut reader, BUSINESS_FRAME_BYTES)
            .await?
            .ok_or_else(|| {
                HostError::before("Disconnected", "the spawner left before its request")
            })?;
        Ok::<_, HostError>((descriptors, request))
    };
    let (descriptors, request) = tokio::time::timeout(CONTROL_TIMEOUT, submitted)
        .await
        .map_err(|_| HostError::before("Timeout", "no spawn request arrived"))??;
    let id = request["id"].clone();
    let started = async {
        if request["method"] != "spawn" {
            return Err(HostError::before(
                "MethodNotAllowed",
                "a spawner submits one spawn request",
            ));
        }
        let [directory, output]: [std::os::fd::OwnedFd; SPAWN_DESCRIPTORS] = descriptors
            .try_into()
            .map_err(|_| HostError::before("InvalidDescriptors", "wrong descriptor count"))?;
        let spawn: SpawnRequest = parameters(request.get("params").cloned().unwrap_or_default())?;
        host.start_shell(granted, spawn, directory, output).await
    };
    let task = match started.await {
        Ok(task) => task,
        Err(error) => {
            host.state
                .lock()
                .await
                .diagnostics
                .note_error(&error.message);
            let _ = write_within(
                &mut writer,
                &hosted::error(&id, &error),
                BUSINESS_FRAME_BYTES,
                CONTROL_TIMEOUT,
            )
            .await;
            return Err(error);
        }
    };
    let served = async {
        write_within(
            &mut writer,
            &hosted::ok(
                &id,
                json!(SpawnedShell {
                    instance: task.instance.clone(),
                    pid: task.pid,
                    data_token: task.data_token.clone(),
                }),
            ),
            BUSINESS_FRAME_BYTES,
            CONTROL_TIMEOUT,
        )
        .await?;
        #[derive(Deserialize)]
        #[serde(tag = "method", content = "params", rename_all = "snake_case")]
        enum SpawnerRequest {
            Stop { reason: String },
        }
        let mut reported = false;
        loop {
            tokio::select! {
                biased;
                _ = host.shutdown.cancelled() => return Ok(()),
                exit = task.ended(), if !reported => {
                    reported = true;
                    write_within(
                        &mut writer,
                        &json!({"event":"exited","exit":exit}),
                        BUSINESS_FRAME_BYTES,
                        STREAM_WRITE_TIMEOUT,
                    )
                    .await?;
                }
                request = hosted::read_frame(&mut reader, ATTACH_REQUEST_FRAME_BYTES) => match request? {
                    Some(request) => match parameters(request)? {
                        SpawnerRequest::Stop { reason } => task.stop(&reason),
                    },
                    None => return Ok(()),
                },
            }
        }
    };
    let outcome = served.await;
    host.release_shell(&task).await;
    outcome
}

/// The Shell adapter: a task describes what it runs and reads as its state and the end of its
/// output. It is stopped through its spawner, never called.
fn shell_request(task: &ShellTask, method: &str) -> Result<Value, HostError> {
    match method {
        "describe" => Ok(json!(task.describe())),
        "read" => Ok(json!(task.read())),
        "call" | "input" | "status" => Err(HostError::before(
            "UnsupportedCapability",
            "a Shell task is stopped by its own Agent, never called",
        )),
        _ => Err(HostError::before(
            "MethodNotAllowed",
            format!("a Shell reader has no {method:?} method"),
        )),
    }
}

/// Watches a Shell task: the reply carries how it reads now, and its exit is pushed once, the one
/// update of a task that was still running. The watch ends with `gone` once the host forgot the
/// task, or with the reader's EOF or anything it sends.
async fn watch_shell(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: &Arc<Host>,
    task: Arc<ShellTask>,
    id: &Value,
) -> Result<(), HostError> {
    use tokio::io::AsyncBufReadExt;
    let read = task.read();
    let mut has_reported = read.exit.is_some();
    write_within(
        &mut writer,
        &hosted::ok(id, json!({"read":read})),
        BUSINESS_FRAME_BYTES,
        CONTROL_TIMEOUT,
    )
    .await?;
    loop {
        tokio::select! {
            biased;
            _ = host.shutdown.cancelled() => return Ok(()),
            _ = task.ended(), if !has_reported => {
                has_reported = true;
                write_within(
                    &mut writer,
                    &json!({"event":"observation","read":task.read()}),
                    BUSINESS_FRAME_BYTES,
                    STREAM_WRITE_TIMEOUT,
                )
                .await?;
            }
            _ = task.released.cancelled() => {
                let _ = write_within(&mut writer, &json!({"event":"gone"}), BUSINESS_FRAME_BYTES, STREAM_WRITE_TIMEOUT).await;
                return Ok(());
            }
            _ = reader.fill_buf() => return Ok(()),
        }
    }
}

/// What a view shows: a hosted Tick's terminal, or the screen the host draws for a Shell task.
#[derive(Clone)]
enum Viewed {
    Tick(Arc<HostedTick>),
    Shell(Arc<ShellTask>),
}

impl Viewed {
    fn changes(&self) -> watch::Receiver<()> {
        match self {
            Self::Tick(tick) => tick.screen_changes.subscribe(),
            Self::Shell(task) => task.view_changes.subscribe(),
        }
    }

    fn frame_since(&self, shown: Option<&vt100::Screen>) -> (Vec<u8>, vt100::Screen) {
        match self {
            Self::Tick(tick) => tick.screen.lock().unwrap().frame_since(shown),
            Self::Shell(task) => task.frame_since(shown),
        }
    }

    /// Completes once its views can show nothing more: a Tick's terminal closed. A Shell's
    /// screen stays, showing how the task ended.
    async fn ended(&self) {
        match self {
            Self::Tick(tick) => tick.terminal_closed.cancelled().await,
            Self::Shell(_) => std::future::pending().await,
        }
    }
}

/// One view attached to a Tick terminal or a Shell's screen, until EOF, detach, a stalled write
/// or the end of the Tick.
struct ViewLease {
    viewed: Viewed,
    view: u64,
}

impl ViewLease {
    fn acquire(viewed: Viewed, params: Value) -> Result<Self, HostError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Attach {
            size: TerminalSize,
        }
        let Attach { size } = parameters(params)?;
        let (view, followed) = match &viewed {
            Viewed::Tick(tick) => {
                if tick.terminal_closed.is_cancelled() {
                    return Err(HostError::before("TargetGone", "the Tick has ended"));
                }
                let mut views = tick.views.lock().unwrap();
                let view = views.attach(size);
                (view, tick.follow_views(&views))
            }
            Viewed::Shell(task) => (task.change_views(|views| views.attach(size)), Ok(())),
        };
        let lease = Self { viewed, view };
        followed.map(|()| lease)
    }

    fn resize(&self, size: TerminalSize) -> Result<(), HostError> {
        match &self.viewed {
            Viewed::Tick(tick) => tick.change_views(|views| views.resized(self.view, size)),
            Viewed::Shell(task) => {
                task.change_views(|views| views.resized(self.view, size));
                Ok(())
            }
        }
    }

    async fn input(&self, data: &[u8]) -> Result<(), HostError> {
        if data.len() > ATTACH_INPUT_BYTES {
            return Err(HostError::before(
                "InputTooLarge",
                format!("input chunks are at most {ATTACH_INPUT_BYTES} bytes"),
            ));
        }
        let tick = match &self.viewed {
            Viewed::Tick(tick) => tick,
            // A Shell's view is read-only: typing only makes it the view that sizes the screen.
            Viewed::Shell(task) => {
                task.change_views(|views| views.typed(self.view));
                return Ok(());
            }
        };
        // A terminal Tick whose program exited only shows its last screen.
        if let Adapter::Terminal(program) = &tick.adapter
            && program.exit().is_some()
        {
            return Ok(());
        }
        tick.note_activity();
        tick.change_views(|views| views.typed(self.view))?;
        tick.pty
            .write_all(data)
            .await
            .map_err(|error| HostError::after("TerminalUnavailable", error.to_string()))
    }
}

impl Drop for ViewLease {
    fn drop(&mut self) {
        match &self.viewed {
            // The leaving view cannot be told of a failed resize; the terminal then keeps its
            // size.
            Viewed::Tick(tick) => {
                let _ = tick.change_views(|views| views.detach(self.view));
            }
            Viewed::Shell(task) => task.change_views(|views| views.detach(self.view)),
        }
    }
}

/// Serves one attached view: the current frame, then latest-only changes, each naming the screen
/// size it draws, while its input and resizes reach the same screen. Output and input run side
/// by side so neither is cut off mid-frame; whichever ends first ends this view, and the Tick and
/// its other views keep running.
async fn attach_view(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    host: &Arc<Host>,
    lease: ViewLease,
    id: &Value,
) -> Result<(), HostError> {
    let viewed = lease.viewed.clone();
    request_send_buffer(&writer, ATTACH_SEND_BUFFER_BYTES);
    let mut changes = viewed.changes();
    write_within(
        &mut writer,
        &hosted::ok(id, json!({"attached":true})),
        BUSINESS_FRAME_BYTES,
        CONTROL_TIMEOUT,
    )
    .await?;
    let output = async {
        let mut shown = None;
        loop {
            changes.borrow_and_update();
            let (bytes, screen) = viewed.frame_since(shown.as_ref());
            let (rows, columns) = screen.size();
            shown = Some(screen);
            if !bytes.is_empty() {
                write_within(
                    &mut writer,
                    &json!({
                        "event": "frame",
                        "size": TerminalSize { rows, columns },
                        "data": String::from_utf8_lossy(&bytes),
                    }),
                    BUSINESS_FRAME_BYTES,
                    STREAM_WRITE_TIMEOUT,
                )
                .await?;
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
        _ = viewed.ended() => Err(HostError::after("TargetGone", "the Tick has ended")),
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
        let _ = write_within(
            &mut writer,
            &event,
            BUSINESS_FRAME_BYTES,
            STREAM_WRITE_TIMEOUT,
        )
        .await;
    }
    outcome
}

/// Writes one frame unless its reader leaves it unread for `limit`; a stalled reader then loses
/// its connection instead of holding this task.
async fn write_within(
    writer: &mut OwnedWriteHalf,
    frame: &Value,
    frame_bytes: usize,
    limit: Duration,
) -> Result<(), HostError> {
    tokio::time::timeout(limit, hosted::write_frame(writer, frame, frame_bytes))
        .await
        .map_err(|_| HostError::after("SlowReader", "the reader stopped reading"))?
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

/// Reads the start configuration the daemon writes on the host's standard input.
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
            std::io::Error::other("the Tickhost needs its start configuration on standard input")
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

    /// A directory short enough for the Session sockets of the protocol Ticks a test host runs.
    fn session_directory_parent() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("th")
            .tempdir_in("/private/tmp")
            .unwrap()
    }

    fn test_host(directory: &tempfile::TempDir) -> Arc<Host> {
        Arc::new(Host {
            paths: HostPaths {
                runtime: directory.path().join("host"),
            },
            epoch: Uuid::new_v4(),
            controller_key: random_token(),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            started: Instant::now(),
            session_serial: AtomicU64::new(0),
            state: Mutex::new(HostState {
                ticks: HashMap::new(),
                shells: HashMap::new(),
                spawn_grants: SpawnGrants::default(),
                spawners: 0,
                controllers: 0,
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
        })
    }

    #[tokio::test]
    async fn a_launcher_waits_until_its_registration_can_be_published() {
        let directory = session_directory_parent();
        let host = test_host(&directory);
        let request = OpenRequest {
            operation: Uuid::new_v4(),
            owner: owner(),
            owner_revision: 0,
            adapter: TickAdapter::Protocol,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf started > started".into(),
            ],
            cwd: directory.path().to_path_buf(),
            environment: Vec::new(),
            title: None,
        };
        let registry = host.state.lock().await;
        let launching = {
            let host = Arc::clone(&host);
            tokio::spawn(async move { host.launch(&request, 0).await })
        };
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started_without_registration = directory.path().join("started").exists();
        drop(registry);
        let outcome = tokio::time::timeout(CONTROL_TIMEOUT, launching)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.unwrap_err().code, "LaunchFailed");
        assert!(directory.path().join("started").exists());
        assert!(host.state.lock().await.ticks.is_empty());
        assert!(
            !started_without_registration,
            "a launcher ran before its token registration could be published"
        );
    }

    #[tokio::test]
    async fn an_owner_ended_while_its_launch_waits_for_the_registry_never_starts() {
        let directory = session_directory_parent();
        let host = test_host(&directory);
        let request = OpenRequest {
            operation: Uuid::new_v4(),
            owner: owner(),
            owner_revision: 3,
            adapter: TickAdapter::Protocol,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf started > started".into(),
            ],
            cwd: directory.path().to_path_buf(),
            environment: Vec::new(),
            title: None,
        };
        let mut registry = host.state.lock().await;
        let launching = {
            let host = Arc::clone(&host);
            let request = request.clone();
            tokio::spawn(async move { host.launch(&request, 0).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        registry.ended.insert(request.owner.clone(), 3);
        drop(registry);
        let outcome = tokio::time::timeout(CONTROL_TIMEOUT, launching)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.unwrap_err().code, "OwnerEnded");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !directory.path().join("started").exists(),
            "nothing spawned"
        );
        assert!(host.state.lock().await.ticks.is_empty());
    }

    fn owner() -> TickOwner {
        TickOwner {
            root: "root".into(),
            chat: Uuid::new_v4(),
        }
    }

    fn nothing_running() -> HashSet<InstanceRef> {
        HashSet::new()
    }

    fn closed() -> Receipt {
        Receipt::Closed {
            instance: InstanceRef::hosted(Uuid::new_v4()),
        }
    }

    #[test]
    fn receipts_keep_the_newest_operations_for_their_own_owner() {
        let mut receipts = Receipts::default();
        let (mine, other) = (owner(), owner());
        let first = Uuid::new_v4();
        receipts.record(first, &mine, closed(), nothing_running);
        assert_eq!(
            receipts.get(first, &other),
            None,
            "another owner sees nothing"
        );
        receipts.record(first, &other, Receipt::Dispatched, nothing_running);
        let failed = Receipt::Failed(HostError::before("LaunchFailed", "no"));
        receipts.record(first, &mine, failed.clone(), nothing_running);
        assert_eq!(receipts.get(first, &mine), Some(failed));
        assert_eq!(
            receipts.get(first, &other),
            Some(Receipt::Dispatched),
            "the same operation id of another owner is its own receipt"
        );
        for _ in 0..RECEIPT_CAPACITY - 1 {
            receipts.record(Uuid::new_v4(), &mine, closed(), nothing_running);
        }
        assert_eq!(receipts.entries.len(), RECEIPT_CAPACITY);
        assert_eq!(
            receipts.get(first, &mine),
            None,
            "the oldest finished receipt expired"
        );
    }

    /// An expired receipt cannot tell whether its operation ran, so a receipt outlives newer ones
    /// while its operation is unfinished or the Tick it opened still runs.
    #[test]
    fn receipts_of_unfinished_operations_and_running_ticks_outlive_newer_ones() {
        let opened = |instance: &InstanceRef| {
            Receipt::Opened(OpenedTick {
                instance: instance.clone(),
                adapter: TickAdapter::Protocol,
                terminal: HostedTerminal {
                    endpoint: PathBuf::from("/host.sock"),
                    epoch: Uuid::new_v4(),
                    terminal: Uuid::new_v4(),
                },
                business_pid: 1,
                data_token: String::new(),
            })
        };
        let (running, ended) = (
            InstanceRef::hosted(Uuid::new_v4()),
            InstanceRef::hosted(Uuid::new_v4()),
        );
        let still_running = || HashSet::from([running.clone()]);
        let mut receipts = Receipts::default();
        let mine = owner();
        let (unfinished, running_open, ended_open) =
            (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        receipts.record(unfinished, &mine, Receipt::Dispatched, still_running);
        let running_receipt = opened(&running);
        receipts.record(running_open, &mine, running_receipt.clone(), still_running);
        receipts.record(ended_open, &mine, opened(&ended), still_running);
        let newest = (0..RECEIPT_CAPACITY)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        for operation in &newest {
            receipts.record(*operation, &mine, closed(), still_running);
        }
        assert_eq!(receipts.entries.len(), RECEIPT_CAPACITY);
        assert_eq!(receipts.get(unfinished, &mine), Some(Receipt::Dispatched));
        assert_eq!(receipts.get(running_open, &mine), Some(running_receipt));
        assert_eq!(receipts.get(ended_open, &mine), None);
        assert_eq!(receipts.get(newest[1], &mine), None);
        assert!(receipts.get(newest[2], &mine).is_some());
    }

    #[tokio::test]
    async fn an_open_of_a_fenced_controller_that_reaches_the_registry_late_never_starts() {
        let directory = session_directory_parent();
        let host = test_host(&directory);
        let request = OpenRequest {
            operation: Uuid::new_v4(),
            owner: owner(),
            owner_revision: 0,
            adapter: TickAdapter::Protocol,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf started > started".into(),
            ],
            cwd: directory.path().to_path_buf(),
            environment: Vec::new(),
            title: None,
        };
        let mut registry = host.state.lock().await;
        let launching = {
            let host = Arc::clone(&host);
            tokio::spawn(async move { host.launch(&request, 0).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A newer daemon takes over while the open waits for the registry.
        registry.controller_generation += 1;
        drop(registry);
        let outcome = tokio::time::timeout(CONTROL_TIMEOUT, launching)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.unwrap_err().code, "Fenced");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !directory.path().join("started").exists(),
            "nothing spawned"
        );
        assert!(host.state.lock().await.ticks.is_empty());
    }

    /// Once a complete snapshot of a root at revision S is applied, every open that can still
    /// start for that root comes from its sender at S or later, so owners ended before S no
    /// longer need a fence; later ends and other roots keep theirs.
    #[tokio::test]
    async fn a_complete_newer_snapshot_forgets_the_owners_its_root_ended_before_it() {
        let directory = tempfile::tempdir().unwrap();
        let host = test_host(&directory);
        let elsewhere = TickOwner {
            root: "elsewhere".into(),
            chat: Uuid::new_v4(),
        };
        let (before, after) = (owner(), owner());
        for (ended, revision) in [(&before, 4), (&after, 5), (&elsewhere, 2)] {
            host.end_owner(ended.clone(), revision, Uuid::new_v4())
                .await
                .unwrap();
        }
        let chunk = |chunk| OwnerSnapshotChunk {
            root: "root".into(),
            revision: 5,
            chunk,
            chunks: 2,
            owners: Vec::new(),
        };
        host.reconcile_owners(chunk(0)).await.unwrap();
        assert_eq!(
            host.state.lock().await.ended.len(),
            3,
            "a partial snapshot forgets nothing"
        );
        host.reconcile_owners(chunk(1)).await.unwrap();
        assert_eq!(
            host.state.lock().await.ended,
            HashMap::from([(after, 5), (elsewhere, 2)])
        );
    }

    #[test]
    fn a_display_title_is_cut_to_its_bound_on_a_character_boundary() {
        let fitting = "界".repeat(TITLE_CHARACTERS);
        assert_eq!(display_title(&fitting), fitting);
        let kept = format!("{}界", "x".repeat(TITLE_CHARACTERS - 2));
        let shown = display_title(&format!("{kept}界界 and the rest"));
        assert_eq!(shown, format!("{kept}{TITLE_ELLIPSIS}"));
        assert_eq!(shown.chars().count(), TITLE_CHARACTERS);
        assert_eq!(display_title(&shown), shown, "a shown title stays as it is");
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
