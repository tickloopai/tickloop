//! Shell tasks: commands a Worker approved, run in pipe mode for as long as their current owner
//! connection. A planned successor can take ownership without spawning a new process. The host
//! judges no command: it runs the one request that carries a live
//! spawn grant and matches what that grant binds, in the directory and with the output file the
//! request passed as descriptors, and never resolves a path of its own. Views of a task show the
//! screen the host draws from its output; one its Worker's daemon granted can keep showing an
//! ended task after its spawner left.

use std::{
    collections::HashMap,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::FileExt, process::ExitStatusExt},
    },
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        self, BUSINESS_FRAME_BYTES, CAPABILITY_SHELL, CAPABILITY_SHELL_HANDOFF, CONTROL_TIMEOUT,
        HostConnection, Role, same_secret,
    },
};
use tokio::{
    io::BufReader,
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
    process::Child,
    sync::watch,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    shell_screen::{self, ShellScreen},
    views::{ViewGrants, Views},
};

/// How long a spawn grant waits for its spawner to connect.
pub const SPAWN_GRANT_TIMEOUT: Duration = Duration::from_secs(10);
/// A committed but unconfirmed Worker transfer may reconnect before its Shell owner is lost.
pub const SHELL_HANDOFF_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// A stopped task's process group gets SIGTERM, then SIGKILL once this passed.
pub const SHELL_TERMINATE_GRACE: Duration = Duration::from_millis(200);
/// The most output a task may write; a task that writes more is stopped and its file cut here.
pub const MAX_SHELL_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// How much of the output's end a read and the exit update carry.
pub const OUTPUT_TAIL_BYTES: usize = 4096;
/// How much of the output's end a view is drawn from when it starts over, as after a resize.
pub const VIEW_OUTPUT_BYTES: u64 = 256 * 1024;
pub const CANCELLED_REASON: &str = "cancelled";
pub const OUTPUT_LIMIT_REASON: &str = "output_limit_exceeded";
pub const UNRESOLVED_DESCENDANTS_REASON: &str =
    "shell exited while its process group still contains descendants; their result is unresolved";
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(40);
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_millis(10);
/// How long a SIGKILL of the whole group may take to show.
const KILL_CONFIRMATION: Duration = Duration::from_secs(1);

/// Who submits a task: the Agent daemon epoch, the Worker it granted, and the Chat.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShellIdentity {
    pub daemon_epoch: Uuid,
    pub worker: Uuid,
    pub chat: Uuid,
}

/// What the Worker approved and checked last: the shell, the command line, the directory it named
/// and the [`environment_digest`] of the environment it starts with. The directory itself travels
/// as a descriptor, the environment with the request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShellApproval {
    pub shell: PathBuf,
    pub command: String,
    pub cwd: PathBuf,
    pub environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<Box<crate::sandbox::SandboxPolicy>>,
}

/// How a spawn grant binds an environment: SHA-256 over its variables in sorted order, each name
/// and value prefixed with its length, in hex.
pub fn environment_digest(environment: &[(Vec<u8>, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut variables = environment.iter().collect::<Vec<_>>();
    variables.sort();
    let mut digest = Sha256::new();
    for (name, value) in variables {
        for part in [name, value] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What one spawn grant admits: one operation of one Worker and Chat, with what was approved.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpawnBinding {
    pub operation: Uuid,
    pub identity: ShellIdentity,
    pub approval: ShellApproval,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SpawnGrant {
    pub grant: Uuid,
    pub token: String,
    /// The host process that issued it; only that one admits it.
    pub epoch: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnRequest {
    pub binding: SpawnBinding,
    pub description: Option<String>,
    /// The environment the command starts with, as the Worker captured it.
    pub environment: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SpawnedShell {
    pub instance: InstanceRef,
    pub pid: u32,
    /// The reader token of its `shell:` instance, for its Worker's own reads and watches.
    pub data_token: String,
    /// A private, one-use token for a planned successor of the submitting Worker.
    #[serde(default)]
    pub owner_token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShellHandoff {
    pub endpoint: PathBuf,
    pub instance: InstanceRef,
    pub owner_token: String,
    pub handoff: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellOutcome {
    Completed,
    Failed,
    /// The host could not tell how the task ended, or what it left running.
    Unknown,
}

/// How a task ended.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShellExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub outcome: ShellOutcome,
    pub ended_at_ms: u64,
}

impl ShellExit {
    /// The status a task shows once it ended: its exit code or signal, or why it stopped.
    pub fn status(&self) -> String {
        if self.outcome == ShellOutcome::Unknown {
            "unknown".into()
        } else if let Some(signal) = self.signal {
            format!("signal {signal}")
        } else if let Some(code) = self.code {
            format!("exit {code}")
        } else if self.reason.is_some() {
            "stopped".into()
        } else {
            "unknown".into()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ShellDescription {
    pub description: Option<String>,
    pub command: String,
    pub cwd: PathBuf,
}

/// A Shell task as it reads: running, or how it ended, with the end of its output.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ShellRead {
    /// 0 while it runs and 1 once it ended, its one update.
    pub seq: u64,
    pub at: String,
    pub task_id: Uuid,
    pub exit: Option<ShellExit>,
    pub output_bytes: u64,
    /// The last [`OUTPUT_TAIL_BYTES`] of its output, decoded lossily.
    pub output: String,
    pub truncated: bool,
}

impl ShellRead {
    /// The content an Agent is given.
    pub fn data(&self) -> Value {
        let (status, state) = match &self.exit {
            None => ("running".to_owned(), json!("running")),
            Some(exit) => (exit.status(), json!(exit.outcome)),
        };
        json!({
            "task_id": self.task_id,
            "status": status,
            "state": state,
            "exit_code": self.exit.as_ref().and_then(|exit| exit.code),
            "signal": self.exit.as_ref().and_then(|exit| exit.signal),
            "reason": self.exit.as_ref().and_then(|exit| exit.reason.clone()),
            "output_bytes": self.output_bytes,
            "output": self.output,
            "truncated": self.truncated,
        })
    }
}

/// A spawner's side of the connection that runs its task.
pub struct SubmittedShell {
    pub spawned: SpawnedShell,
    pub exits: ShellExits,
    pub stops: ShellStops,
}

/// Where a submitted task's exit arrives, once.
pub struct ShellExits {
    reader: BufReader<OwnedReadHalf>,
}

impl ShellExits {
    /// The task's exit; an error once the connection ended without one, which leaves the
    /// outcome unknown.
    pub async fn exited(&mut self) -> Result<ShellExit, HostError> {
        let event = hosted::read_frame(&mut self.reader, BUSINESS_FRAME_BYTES)
            .await?
            .ok_or_else(|| {
                HostError::after("Disconnected", "the Tickhost left before the task ended")
            })?;
        if event["event"] != "exited" {
            return Err(HostError::after(
                "ProtocolViolation",
                "the Tickhost sent something other than the task's exit",
            ));
        }
        serde_json::from_value(event["exit"].clone())
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }
}

/// How the current owner stops its task. A transferred previous owner's requests are fenced.
pub struct ShellStops {
    writer: OwnedWriteHalf,
}

impl ShellStops {
    pub async fn commit_handoff(&mut self) -> Result<(), HostError> {
        hosted::write_frame(
            &mut self.writer,
            &json!({"method":"confirm_handoff"}),
            BUSINESS_FRAME_BYTES,
        )
        .await
    }

    pub async fn stop(&mut self, reason: &str) -> Result<(), HostError> {
        hosted::write_frame(
            &mut self.writer,
            &json!({"method":"stop","params":{"reason":reason}}),
            BUSINESS_FRAME_BYTES,
        )
        .await
    }
}

/// Submits `request` under `grant`: the approved directory and the output file go first as
/// descriptors, then the request, on one new spawner connection the task then lives on.
pub async fn submit(
    endpoint: &std::path::Path,
    grant: &SpawnGrant,
    request: &SpawnRequest,
    directory: &std::fs::File,
    output: &std::fs::File,
) -> Result<SubmittedShell, HostError> {
    submit_checked(endpoint, grant, request, directory, output, || Ok(())).await
}

pub async fn submit_checked(
    endpoint: &std::path::Path,
    grant: &SpawnGrant,
    request: &SpawnRequest,
    directory: &std::fs::File,
    output: &std::fs::File,
    before_dispatch: impl FnOnce() -> Result<(), HostError>,
) -> Result<SubmittedShell, HostError> {
    let mut connection = HostConnection::connect(
        endpoint,
        Role::Spawner {
            grant: grant.grant,
            token: grant.token.clone(),
        },
        &[CAPABILITY_SHELL],
    )
    .await?;
    connection
        .send_descriptors(&[directory.as_raw_fd(), output.as_raw_fd()])
        .await?;
    before_dispatch()?;
    let spawned = connection
        .request("spawn", json!(request), CONTROL_TIMEOUT)
        .await?;
    let spawned = serde_json::from_value(spawned)
        .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))?;
    let (reader, writer) = connection.into_parts();
    Ok(SubmittedShell {
        spawned,
        exits: ShellExits { reader },
        stops: ShellStops { writer },
    })
}

/// Adopts an existing task without spawning a command. A successful hello fences the previous
/// owner's stop requests and disconnect, and rotates the token before returning it.
pub async fn adopt(handoff: &ShellHandoff) -> Result<SubmittedShell, HostError> {
    let mut connection = HostConnection::connect(
        &handoff.endpoint,
        Role::ShellSuccessor {
            instance: handoff.instance.clone(),
            owner_token: handoff.owner_token.clone(),
            handoff: handoff.handoff,
        },
        &[CAPABILITY_SHELL_HANDOFF],
    )
    .await?;
    let spawned = connection
        .request("adopt", json!({}), CONTROL_TIMEOUT)
        .await?;
    let spawned = serde_json::from_value(spawned)
        .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))?;
    let (reader, writer) = connection.into_parts();
    Ok(SubmittedShell {
        spawned,
        exits: ShellExits { reader },
        stops: ShellStops { writer },
    })
}

/// Spawn grants not yet claimed. Each admits one spawner once before it expires.
#[derive(Default)]
pub(crate) struct SpawnGrants {
    pending: HashMap<Uuid, PendingSpawn>,
}

struct PendingSpawn {
    token: String,
    binding: SpawnBinding,
    expires: Instant,
}

impl SpawnGrants {
    pub(crate) fn issue(&mut self, binding: SpawnBinding, token: String, now: Instant) -> Uuid {
        self.pending.retain(|_, pending| now < pending.expires);
        let grant = tickloop_ids::generate();
        self.pending.insert(
            grant,
            PendingSpawn {
                token,
                binding,
                expires: now + SPAWN_GRANT_TIMEOUT,
            },
        );
        grant
    }

    /// Takes the grant `grant` names when `token` proves it and it has not expired; a grant
    /// works once.
    pub(crate) fn claim(&mut self, grant: Uuid, token: &str, now: Instant) -> Option<SpawnBinding> {
        if !self
            .pending
            .get(&grant)
            .is_some_and(|pending| same_secret(token, &pending.token))
        {
            return None;
        }
        self.pending
            .remove(&grant)
            .filter(|pending| now < pending.expires)
            .map(|pending| pending.binding)
    }

    /// How many grants no spawner claimed yet and that have not expired.
    pub(crate) fn pending(&mut self, now: Instant) -> usize {
        self.pending.retain(|_, pending| now < pending.expires);
        self.pending.len()
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }
}

/// One running or ended Shell task.
pub(crate) struct ShellTask {
    pub(crate) binding: SpawnBinding,
    description: Option<String>,
    pub(crate) instance: InstanceRef,
    pub(crate) data_token: String,
    pub(crate) pid: u32,
    ownership: std::sync::Mutex<ShellOwnership>,
    output: std::fs::File,
    network_proxy: Option<crate::sandbox::NetworkSession>,
    /// The first reason it was asked to stop for.
    stop: watch::Sender<Option<String>>,
    exit: watch::Sender<Option<ShellExit>>,
    /// Cancelled once the task ended and its spawner left: its readers end, and only views still
    /// show it.
    pub(crate) released: CancellationToken,
    view: std::sync::Mutex<TaskView>,
    pub(crate) view_changes: watch::Sender<()>,
    views: std::sync::Mutex<Views>,
    pub(crate) view_grants: std::sync::Mutex<ViewGrants>,
    /// When it was last released or left without views; a released task nobody views is
    /// forgotten after the host's idle timeout.
    unviewed_since_ms: AtomicU64,
}

struct ShellOwnership {
    generation: u64,
    token: String,
    released: bool,
    previous: Option<(String, Uuid)>,
    connected: bool,
    is_handoff_pending: bool,
}

/// The screen a task's views show, how far its output was read into it, and how far back each
/// view that scrolled shows it; a view not listed follows the newest output.
struct TaskView {
    screen: ShellScreen,
    read_to: u64,
    scrolled_back: HashMap<u64, usize>,
}

impl TaskView {
    /// Keeps each scrolled view within the history the screen now holds.
    fn clamp_scrolls(&mut self) {
        let history = self.screen.history_rows();
        self.scrolled_back.retain(|_, back| {
            *back = (*back).min(history);
            *back > 0
        });
    }
}

impl ShellTask {
    /// Starts `request` as `<shell> -c <command>` in its own process group: standard input
    /// empty, standard output and error appended to the passed output file, and the working
    /// directory the passed directory descriptor, entered right before the shell runs.
    pub(crate) fn start(
        request: SpawnRequest,
        directory: OwnedFd,
        output: OwnedFd,
        data_token: String,
        network_proxy: Option<crate::sandbox::NetworkSession>,
    ) -> Result<Arc<Self>, HostError> {
        let SpawnRequest {
            binding,
            description,
            environment,
        } = request;
        if !binding.approval.shell.is_absolute() {
            return Err(HostError::before(
                "InvalidArguments",
                "the shell must be an absolute path",
            ));
        }
        check_kind(&directory, libc::S_IFDIR, "working directory")?;
        check_kind(&output, libc::S_IFREG, "output file")?;
        let output = std::fs::File::from(output);
        let launch_failed =
            |error: std::io::Error| HostError::before("LaunchFailed", error.to_string());
        let mut command = match &binding.approval.sandbox {
            Some(policy) => {
                crate::sandbox::command(policy, &binding.approval.shell).map_err(launch_failed)?
            }
            None => tokio::process::Command::new(&binding.approval.shell),
        };
        command
            .arg("-c")
            .arg(&binding.approval.command)
            .env_clear()
            .envs(environment.iter().map(|(name, value)| {
                (
                    std::ffi::OsStr::from_bytes(name),
                    std::ffi::OsStr::from_bytes(value),
                )
            }))
            .stdin(Stdio::null())
            .stdout(output.try_clone().map_err(launch_failed)?)
            .stderr(output.try_clone().map_err(launch_failed)?)
            .process_group(0)
            .kill_on_drop(true);
        if let Some(proxy) = &network_proxy {
            proxy.configure(&mut command);
        }
        let directory_descriptor = directory.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(directory_descriptor) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = hosted::spawn_child(|| command.spawn()).map_err(launch_failed)?;
        drop(directory);
        let pid = child.id().ok_or_else(|| {
            HostError::before("LaunchFailed", "the shell exited before it started")
        })?;
        let view = TaskView {
            screen: ShellScreen::new(
                description.clone(),
                binding.approval.command.clone(),
                binding.approval.cwd.display().to_string(),
            ),
            read_to: 0,
            scrolled_back: HashMap::new(),
        };
        let task = Arc::new(Self {
            instance: InstanceRef::shell(binding.operation),
            binding,
            description,
            data_token,
            pid,
            ownership: std::sync::Mutex::new(ShellOwnership {
                generation: 0,
                token: crate::host::random_token(),
                released: false,
                previous: None,
                connected: true,
                is_handoff_pending: false,
            }),
            output,
            network_proxy,
            stop: watch::channel(None).0,
            exit: watch::channel(None).0,
            released: CancellationToken::new(),
            view: std::sync::Mutex::new(view),
            view_changes: watch::channel(()).0,
            views: std::sync::Mutex::new(Views::default()),
            view_grants: std::sync::Mutex::new(ViewGrants::default()),
            unviewed_since_ms: AtomicU64::new(now_ms()),
        });
        tokio::spawn(supervise(task.clone(), child));
        Ok(task)
    }

    pub(crate) fn spawned(&self) -> SpawnedShell {
        SpawnedShell {
            instance: self.instance.clone(),
            pid: self.pid,
            data_token: self.data_token.clone(),
            owner_token: self.ownership.lock().unwrap().token.clone(),
        }
    }

    pub(crate) fn can_adopt(&self, token: &str, handoff: Uuid) -> bool {
        let owner = self.ownership.lock().unwrap();
        !owner.released
            && (same_secret(token, &owner.token)
                || owner
                    .previous
                    .as_ref()
                    .is_some_and(|(previous, operation)| {
                        *operation == handoff && same_secret(token, previous)
                    }))
    }

    pub(crate) fn adopt(&self, token: &str, handoff: Uuid) -> Result<u64, HostError> {
        let mut owner = self.ownership.lock().unwrap();
        let is_retry = owner
            .previous
            .as_ref()
            .is_some_and(|(previous, operation)| {
                *operation == handoff && same_secret(token, previous)
            });
        if owner.released || (!same_secret(token, &owner.token) && !is_retry) {
            return Err(HostError::before(
                "Forbidden",
                "unknown or already used Shell owner token",
            ));
        }
        owner.generation = owner
            .generation
            .checked_add(1)
            .ok_or_else(|| HostError::before("Fenced", "Shell owner generation exhausted"))?;
        if !is_retry {
            owner.previous = Some((owner.token.clone(), handoff));
            owner.token = crate::host::random_token();
        }
        owner.connected = true;
        owner.is_handoff_pending = true;
        Ok(owner.generation)
    }

    pub(crate) fn stop_owned(&self, generation: u64, reason: &str) {
        let owner = self.ownership.lock().unwrap();
        if owner.generation == generation && !owner.released {
            self.stop(reason);
        }
    }

    pub(crate) fn confirm_owner(&self, generation: u64) {
        let mut owner = self.ownership.lock().unwrap();
        if owner.generation == generation && !owner.released {
            owner.is_handoff_pending = false;
        }
    }

    pub(crate) fn disconnect_owner(&self, generation: u64) -> Option<Duration> {
        let mut owner = self.ownership.lock().unwrap();
        if owner.generation != generation || owner.released {
            return None;
        }
        owner.connected = false;
        Some(if owner.is_handoff_pending {
            SHELL_HANDOFF_RECOVERY_TIMEOUT
        } else {
            Duration::ZERO
        })
    }

    pub(crate) fn release_disconnected_owner(&self, generation: u64) -> bool {
        let mut owner = self.ownership.lock().unwrap();
        if owner.generation != generation || owner.released || owner.connected {
            return false;
        }
        owner.released = true;
        self.stop(CANCELLED_REASON);
        true
    }

    /// Asks the task to stop for `reason`; a task that ended, or was asked already, keeps its
    /// first reason.
    pub(crate) fn stop(&self, reason: &str) {
        self.stop.send_if_modified(|current| {
            let is_first = current.is_none() && self.exit.borrow().is_none();
            if is_first {
                *current = Some(reason.to_owned());
            }
            is_first
        });
    }

    pub(crate) fn exit(&self) -> Option<ShellExit> {
        self.exit.borrow().clone()
    }

    pub(crate) async fn ended(&self) -> ShellExit {
        let mut exit = self.exit.subscribe();
        let ended = exit
            .wait_for(Option::is_some)
            .await
            .expect("a task keeps its own exit channel")
            .clone();
        ended.expect("waited for an exit")
    }

    pub(crate) fn describe(&self) -> ShellDescription {
        ShellDescription {
            description: self.description.clone(),
            command: self.binding.approval.command.clone(),
            cwd: self.binding.approval.cwd.clone(),
        }
    }

    pub(crate) fn read(&self) -> ShellRead {
        let exit = self.exit();
        let output_bytes = self.output_length();
        let start = output_bytes.saturating_sub(OUTPUT_TAIL_BYTES as u64);
        let mut tail = vec![0u8; (output_bytes - start) as usize];
        let read = self.output.read_at(&mut tail, start).unwrap_or(0);
        tail.truncate(read);
        ShellRead {
            seq: u64::from(exit.is_some()),
            at: tick_protocol::now_rfc3339(),
            task_id: self.binding.operation,
            exit,
            output_bytes,
            output: String::from_utf8_lossy(&tail).into_owned(),
            truncated: start > 0,
        }
    }

    fn output_length(&self) -> u64 {
        self.output.metadata().map_or(0, |metadata| metadata.len())
    }

    /// Reads what the task wrote since its view last did into that view, starting over from the
    /// end of the output when it fell far behind.
    /// A view scrolled back keeps showing the same lines as more arrive. The running time in the
    /// header moves on too.
    fn follow_output(&self) {
        let length = self.output_length();
        let mut view = self.view.lock().unwrap();
        let ticked = view.screen.tick(Instant::now());
        if length <= view.read_to {
            drop(view);
            if ticked {
                self.view_changes.send_replace(());
            }
            return;
        }
        if length - view.read_to > VIEW_OUTPUT_BYTES {
            let size = view.screen.size();
            let (tail, read_to) = self.output_end(length);
            view.screen.redraw(size, &tail);
            view.read_to = read_to;
            view.clamp_scrolls();
        } else {
            let mut added = vec![0u8; (length - view.read_to) as usize];
            let read = self.output.read_at(&mut added, view.read_to).unwrap_or(0);
            let pushed = view.screen.append(&added[..read]);
            view.read_to += read as u64;
            for back in view.scrolled_back.values_mut() {
                *back += pushed;
            }
            view.clamp_scrolls();
        }
        drop(view);
        self.view_changes.send_replace(());
    }

    /// Moves `view`'s place in the output by the scroll keys in `input`.
    pub(crate) fn scroll(&self, view: u64, input: &[u8]) {
        let scrolls = shell_screen::scroll_keys(input);
        if scrolls.is_empty() {
            return;
        }
        let mut task_view = self.view.lock().unwrap();
        let history = task_view.screen.history_rows();
        let page = task_view.screen.output_rows();
        let mut back = task_view.scrolled_back.get(&view).copied().unwrap_or(0);
        for scroll in scrolls {
            back = shell_screen::scrolled(back, scroll, history, page);
        }
        if back == 0 {
            task_view.scrolled_back.remove(&view);
        } else {
            task_view.scrolled_back.insert(view, back);
        }
        drop(task_view);
        self.view_changes.send_replace(());
    }

    /// Forgets where `view` scrolled to, once it detached.
    pub(crate) fn forget_scroll(&self, view: u64) {
        self.view.lock().unwrap().scrolled_back.remove(&view);
    }

    /// The last [`VIEW_OUTPUT_BYTES`] of the first `length` output bytes, from the start of a
    /// line when it was cut, and where they end.
    fn output_end(&self, length: u64) -> (Vec<u8>, u64) {
        let start = length.saturating_sub(VIEW_OUTPUT_BYTES);
        let mut tail = vec![0u8; (length - start) as usize];
        let read = self.output.read_at(&mut tail, start).unwrap_or(0);
        tail.truncate(read);
        if start > 0
            && let Some(line_end) = tail.iter().position(|byte| *byte == b'\n')
        {
            tail.drain(..=line_end);
        }
        (tail, start + read as u64)
    }

    /// Shows how the task ended on its view.
    fn show_exit(&self, exit: &ShellExit) {
        self.view.lock().unwrap().screen.end(
            exit.status(),
            exit.reason.as_deref(),
            exit.outcome == ShellOutcome::Completed,
        );
        self.view_changes.send_replace(());
    }

    /// The terminal output that brings `view`, showing `shown`, to the task's screen now at its
    /// place in the output, with the screen it then shows.
    pub(crate) fn frame_since(
        &self,
        view: u64,
        shown: Option<&vt100::Screen>,
    ) -> (Vec<u8>, vt100::Screen) {
        let mut task_view = self.view.lock().unwrap();
        let back = task_view.scrolled_back.get(&view).copied().unwrap_or(0);
        task_view.screen.frame_since(back, shown)
    }

    pub(crate) fn view_count(&self) -> usize {
        self.views.lock().unwrap().len()
    }

    /// Applies `change` to the attached views and draws the screen at the size they now lead
    /// with.
    pub(crate) fn change_views<T>(&self, change: impl FnOnce(&mut Views) -> T) -> T {
        let mut views = self.views.lock().unwrap();
        let changed = change(&mut views);
        if views.is_empty() {
            self.unviewed_since_ms.store(now_ms(), Ordering::SeqCst);
        }
        let Some(size) = views.leading_size() else {
            return changed;
        };
        let mut view = self.view.lock().unwrap();
        if view.screen.size() != size {
            let (tail, read_to) = self.output_end(self.output_length());
            view.screen.redraw(size, &tail);
            view.read_to = read_to;
            view.clamp_scrolls();
            drop(view);
            self.view_changes.send_replace(());
        }
        changed
    }

    /// Ends its readers once its spawner left; its views keep it a while.
    pub(crate) fn release(&self) {
        self.unviewed_since_ms.store(now_ms(), Ordering::SeqCst);
        self.released.cancel();
    }

    /// Whether the host can forget it: released, and nobody viewed it for `idle_timeout`.
    pub(crate) fn is_forgotten(&self, now_ms: u64, idle_timeout: Duration) -> bool {
        self.released.is_cancelled()
            && self.views.lock().unwrap().is_empty()
            && now_ms.saturating_sub(self.unviewed_since_ms.load(Ordering::SeqCst))
                >= idle_timeout.as_millis() as u64
    }

    /// Cuts the output at its limit; whether it had written more.
    fn cut_output(&self) -> bool {
        let is_over = self.output_length() > MAX_SHELL_OUTPUT_BYTES;
        if is_over {
            let _ = self.output.set_len(MAX_SHELL_OUTPUT_BYTES);
        }
        is_over
    }
}

fn check_kind(descriptor: &OwnedFd, kind: libc::mode_t, name: &str) -> Result<(), HostError> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(descriptor.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(HostError::before(
            "InvalidDescriptors",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    if unsafe { status.assume_init() }.st_mode & libc::S_IFMT != kind {
        return Err(HostError::before(
            "InvalidDescriptors",
            format!("the passed {name} has the wrong file type"),
        ));
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Follows a task until it ends: a stop request, or output beyond the limit, terminates its group.
/// Its exit is published only once nothing of its group runs and its output is saved.
async fn supervise(task: Arc<ShellTask>, mut child: Child) {
    let pid = task.pid;
    let mut stop = task.stop.subscribe();
    let mut output_poll = tokio::time::interval(OUTPUT_POLL_INTERVAL);
    output_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    output_poll.tick().await;
    enum Ending {
        Exited(std::io::Result<std::process::ExitStatus>),
        Stopping(String),
    }
    let ending = loop {
        tokio::select! {
            status = child.wait() => break Ending::Exited(status),
            requested = stop.wait_for(Option::is_some) => {
                let reason = requested.ok().and_then(|reason| reason.clone());
                break Ending::Stopping(reason.unwrap_or_else(|| CANCELLED_REASON.into()));
            }
            _ = output_poll.tick() => {
                if task.cut_output() {
                    break Ending::Stopping(OUTPUT_LIMIT_REASON.into());
                }
                task.follow_output();
            }
        }
    };
    let (status, reason) = match ending {
        Ending::Exited(status) => (status, None),
        Ending::Stopping(reason) => (terminate(&mut child, pid).await, Some(reason)),
    };
    let mut exit = match status {
        Ok(status) => {
            let (code, signal) = (status.code(), status.signal());
            ShellExit {
                outcome: if code == Some(0) && signal.is_none() && reason.is_none() {
                    ShellOutcome::Completed
                } else {
                    ShellOutcome::Failed
                },
                code,
                signal,
                reason,
                ended_at_ms: 0,
            }
        }
        Err(error) => ShellExit {
            code: None,
            signal: None,
            reason: Some(error.to_string()),
            outcome: ShellOutcome::Unknown,
            ended_at_ms: 0,
        },
    };
    if process_group_exists(pid) {
        exit.outcome = ShellOutcome::Unknown;
        exit.reason = Some(UNRESOLVED_DESCENDANTS_REASON.into());
        terminate_process_group(pid).await;
    }
    if let Some(proxy) = &task.network_proxy {
        proxy.stop();
    }
    if task.cut_output() && exit.reason.is_none() {
        exit.reason = Some(OUTPUT_LIMIT_REASON.into());
        exit.outcome = ShellOutcome::Failed;
    }
    if let Err(error) = task.output.sync_all() {
        exit.outcome = ShellOutcome::Unknown;
        exit.reason = Some(format!("cannot save command output: {error}"));
    }
    if task.binding.approval.sandbox.is_some() && exit.outcome == ShellOutcome::Failed {
        crate::sandbox::annotate_denial(&task.output);
    }
    if let Some(proxy) = &task.network_proxy {
        proxy.annotate_denials(&task.output);
    }
    exit.ended_at_ms = now_ms();
    task.follow_output();
    task.show_exit(&exit);
    task.exit.send_replace(Some(exit));
    // Views tell their clients the task ended once its exit is readable.
    task.view_changes.send_replace(());
}

async fn terminate(child: &mut Child, pid: u32) -> std::io::Result<std::process::ExitStatus> {
    let deadline = tokio::time::Instant::now() + SHELL_TERMINATE_GRACE;
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
    let status = tokio::time::timeout_at(deadline, child.wait()).await;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
    if process_group_exists(pid) {
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
    }
    let status = match status {
        Ok(status) => status,
        Err(_) => tokio::time::timeout(KILL_CONFIRMATION, child.wait())
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "cannot confirm command termination",
                )
            })?,
    };
    let deadline = tokio::time::Instant::now() + SHELL_TERMINATE_GRACE;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
    status
}

async fn terminate_process_group(pid: u32) {
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = tokio::time::Instant::now() + SHELL_TERMINATE_GRACE;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
    if process_group_exists(pid) {
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
    }
    let deadline = tokio::time::Instant::now() + KILL_CONFIRMATION;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
}

pub fn process_group_exists(pid: u32) -> bool {
    (unsafe { libc::killpg(pid as libc::pid_t, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> SpawnBinding {
        SpawnBinding {
            operation: Uuid::new_v4(),
            identity: ShellIdentity {
                daemon_epoch: Uuid::new_v4(),
                worker: Uuid::new_v4(),
                chat: Uuid::new_v4(),
            },
            approval: ShellApproval {
                shell: "/bin/sh".into(),
                command: "true".into(),
                cwd: "/".into(),
                environment: environment_digest(&[]),
                sandbox: None,
            },
        }
    }

    #[test]
    fn an_environment_digest_ignores_the_order_of_variables_but_not_where_one_ends() {
        let variable =
            |name: &str, value: &str| (name.as_bytes().to_vec(), value.as_bytes().to_vec());
        assert_eq!(
            environment_digest(&[variable("A", "1"), variable("B", "2")]),
            environment_digest(&[variable("B", "2"), variable("A", "1")])
        );
        assert_ne!(
            environment_digest(&[variable("AB", "C")]),
            environment_digest(&[variable("A", "BC")])
        );
        assert_ne!(
            environment_digest(&[variable("A", "1")]),
            environment_digest(&[variable("A", "1"), variable("B", "")])
        );
    }

    #[test]
    fn a_spawn_grant_admits_its_own_token_once_before_it_expires() {
        let mut grants = SpawnGrants::default();
        let now = Instant::now();
        let bound = binding();
        let grant = grants.issue(bound.clone(), "secret".into(), now);
        assert_eq!(grants.claim(grant, "guess", now), None);
        assert_eq!(grants.claim(Uuid::new_v4(), "secret", now), None);
        assert_eq!(grants.claim(grant, "secret", now), Some(bound));
        assert_eq!(grants.claim(grant, "secret", now), None);

        let late = grants.issue(binding(), "secret".into(), now);
        assert_eq!(grants.pending(now), 1);
        assert_eq!(
            grants.claim(late, "secret", now + SPAWN_GRANT_TIMEOUT),
            None
        );
        assert_eq!(grants.pending(now + SPAWN_GRANT_TIMEOUT), 0);
    }

    #[test]
    fn an_ended_task_reads_as_its_exit_and_the_end_of_its_output() {
        let exit = |code, signal, reason: Option<&str>, outcome| ShellExit {
            code,
            signal,
            reason: reason.map(str::to_owned),
            outcome,
            ended_at_ms: 1,
        };
        assert_eq!(
            exit(Some(0), None, None, ShellOutcome::Completed).status(),
            "exit 0"
        );
        assert_eq!(
            exit(None, Some(15), Some(CANCELLED_REASON), ShellOutcome::Failed).status(),
            "signal 15"
        );
        assert_eq!(
            exit(None, None, Some("lost"), ShellOutcome::Unknown).status(),
            "unknown"
        );
        let read = ShellRead {
            seq: 1,
            at: "2026-10-13T00:00:00Z".into(),
            task_id: Uuid::nil(),
            exit: Some(exit(Some(7), None, None, ShellOutcome::Failed)),
            output_bytes: 3,
            output: "out".into(),
            truncated: false,
        };
        assert_eq!(
            read.data(),
            json!({"task_id":Uuid::nil(),"status":"exit 7","state":"failed","exit_code":7,"signal":null,"reason":null,"output_bytes":3,"output":"out","truncated":false})
        );
    }
}
