//! The Tickhost wire contract shared by the host service, its Agent daemon client, data clients,
//! the hosted launcher and the Shell task spawner. Frames are one JSON object per line on a
//! private Unix socket. The first frame names the channel: the frozen management channel only
//! says hello and shuts the host down; the business channel negotiates its minor version and
//! capabilities and then serves exactly one authenticated role.

use std::{
    os::fd::{AsRawFd, RawFd},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::host_error::HostError;
use tick_protocol::InstanceRef;

/// Product version for diagnostics; protocol versions and capabilities determine compatibility.
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MANAGEMENT_MAJOR: u16 = 1;
pub const MANAGEMENT_MINOR: u16 = 0;
pub const MANAGEMENT_FRAME_BYTES: usize = 64 * 1024;
/// 3.0: owner revisions on open, listings that name their host epoch, owner reconcile/end,
/// Watch-intent leases, explicit or automatic reads, and one-time view grants. 4.0: the
/// controller role carries `daemon_epoch` and the launch binding is read from
/// `TICKLOOP_TICKHOST_LAUNCH`. 5.0: several views attach to one Tick at once, each naming its
/// size, every frame names the screen size it draws, every view grant admits its own view, and
/// listings count each Tick's views. 6.0: an open names its adapter, protocol or terminal, as do
/// its answer and the listings; a terminal Tick reads as screen text with its program's exit, is
/// written to with `input` and is watched only until it ends. 7.0: the controller asks for
/// one-time spawn grants, a spawner connection runs one Shell task from the descriptors and
/// launch it passes, and the Shell adapter serves that task's `shell:` instance. 8.0: the
/// controller grants views of a Worker's Shell task, running or ended, which attach to a screen
/// the host draws from the task's output and take no input. 9.0: Ticks no longer idle out, so
/// Watch-intent leases (`set_intents`) and the explicit or automatic kind of a read are gone.
/// 10.0: a terminal Tick's watch reads its screen with `read` and pushes `changed` once when the
/// text first differs from the screen it last read, and `exited` with how the program ended. A
/// host of any other major is refused at hello.
pub const BUSINESS_MAJOR: u16 = 10;
/// 10.1: optional immutable Shell sandbox policy in the single-use spawn authorization.
pub const BUSINESS_MINOR: u16 = 1;
/// A complete business frame, large enough for one legal 8 MiB Tick Tool response.
pub const BUSINESS_FRAME_BYTES: usize = 8 * 1024 * 1024 + 64 * 1024;
/// One pushed observation on a watch stream: a Tick Update of at most
/// [`tick_protocol::MAX_UPDATE_BYTES`] and its envelope.
pub const OBSERVATION_FRAME_BYTES: usize = tick_protocol::MAX_UPDATE_BYTES + 32 * 1024;
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a data client waits for a call's answer: the Tick's whole budget plus transport.
pub const CALL_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(CALL_TIMEOUT.as_secs() + CONTROL_TIMEOUT.as_secs());
/// One input chunk of an attached view; larger input is sent as several chunks in order.
pub const ATTACH_INPUT_BYTES: usize = 4 * 1024;
/// An attached view's request frame: one input chunk, a resize or detach.
pub const ATTACH_REQUEST_FRAME_BYTES: usize = 64 * 1024;
/// The terminal sizes a hosted Tick's PTY accepts; an attached view clamps its own size to them.
pub const MIN_ROWS: u16 = 2;
pub const MAX_ROWS: u16 = 300;
pub const MIN_COLUMNS: u16 = 2;
pub const MAX_COLUMNS: u16 = 500;
/// The launch binding a hosted launcher reads from its environment.
pub const LAUNCH_ENV: &str = "TICKLOOP_TICKHOST_LAUNCH";

pub const CAPABILITY_LIFECYCLE: &str = "lifecycle";
pub const CAPABILITY_IDLE_SHUTDOWN: &str = "idle_shutdown";
pub const CAPABILITY_DATA: &str = "data";
pub const CAPABILITY_LAUNCH: &str = "launch";
pub const CAPABILITY_SHELL: &str = "shell";
pub const CAPABILITY_SHELL_HANDOFF: &str = "shell_handoff";
pub const BUSINESS_CAPABILITIES: [&str; 5] = [
    CAPABILITY_LIFECYCLE,
    CAPABILITY_DATA,
    CAPABILITY_LAUNCH,
    CAPABILITY_SHELL,
    CAPABILITY_SHELL_HANDOFF,
];
/// The descriptors a spawner passes before its request: the approved working directory and the
/// output file.
pub const SPAWN_DESCRIPTORS: usize = 2;
/// The one byte that carries passed descriptors.
const DESCRIPTOR_MARKER: u8 = b'F';

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "channel", rename_all = "snake_case", deny_unknown_fields)]
pub enum Hello {
    Management {
        major: u16,
        minor: u16,
        controller_key: String,
    },
    Business {
        major: u16,
        minor: u16,
        required: Vec<String>,
        role: Role,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Role {
    /// The Agent daemon: the only writer of opens, closes and ownership.
    Controller {
        controller_key: String,
        daemon_epoch: uuid::Uuid,
    },
    /// A reader of one instance, holding the data token issued when it opened.
    Instance {
        instance: InstanceRef,
        data_token: String,
    },
    /// A view of one instance's screen, a Tick's terminal or a Shell task's, holding the one-time
    /// grant its owner asked the host for; it can only attach, so no data token leaves the Agent.
    Viewer {
        instance: InstanceRef,
        view: uuid::Uuid,
    },
    /// The launcher of one pending open, holding that open's one-time launch token.
    Launcher {
        operation: uuid::Uuid,
        launch_token: String,
    },
    /// The submitter of one Shell task, holding the one-time spawn grant its daemon asked the
    /// host for; the task lives as long as this connection.
    Spawner { grant: uuid::Uuid, token: String },
    /// A planned successor presenting the current Shell owner's one-use transfer token.
    ShellSuccessor {
        instance: InstanceRef,
        owner_token: String,
        handoff: uuid::Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HelloReply {
    pub epoch: uuid::Uuid,
    pub pid: u32,
    pub version: String,
    pub major: u16,
    pub minor: u16,
    pub capabilities: Vec<String>,
    /// What the frozen management channel adds for a daemon deciding whether the host is of its
    /// own build; absent from a business reply and from hosts older than these fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostDescription>,
}

/// The build a host runs and what it serves, as its management hello reports it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostDescription {
    /// The content identity of this host and its production dependencies, independent of the
    /// Agent's implementation and the product release version.
    pub build: String,
    pub executable: Option<PathBuf>,
    pub business_major: u16,
    pub business_minor: u16,
    pub occupancy: HostOccupancy,
}

/// What stopping a host would end.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostOccupancy {
    /// Each Tick it holds, by its title, or its instance or command when it has none.
    pub ticks: Vec<String>,
    pub shells: usize,
    /// Opens it has not answered yet and Shell spawn grants no spawner claimed yet, each about
    /// to run something; `None` from a host older than this count, which may be starting one.
    #[serde(default)]
    pub pending: Option<usize>,
    /// Its authenticated business connections: attached views, data clients, its Ticks'
    /// launchers and the daemon's own.
    pub connections: usize,
    /// The views attached to its Ticks and Shell tasks, which stopping it closes; `None` from a
    /// host older than this count.
    #[serde(default)]
    pub views: Option<usize>,
}

/// A connection on the frozen management channel, which any host answers whatever its build.
pub struct ManagementConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    hello: HelloReply,
}

impl ManagementConnection {
    pub async fn connect(endpoint: &Path, controller_key: &str) -> Result<Self, HostError> {
        let owner = verify_endpoint(endpoint)?;
        let stream = tokio::time::timeout(CONTROL_TIMEOUT, UnixStream::connect(endpoint))
            .await
            .map_err(|_| HostError::before("HostUnavailable", "connect timed out"))?
            .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
        if peer_uid(&stream)? != owner {
            return Err(HostError::before(
                "HostUnavailable",
                "Tickhost endpoint belongs to another user",
            ));
        }
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let hello = Hello::Management {
            major: MANAGEMENT_MAJOR,
            minor: MANAGEMENT_MINOR,
            controller_key: controller_key.to_owned(),
        };
        let exchange = async {
            write_frame(&mut writer, &json!(hello), MANAGEMENT_FRAME_BYTES).await?;
            read_frame(&mut reader, MANAGEMENT_FRAME_BYTES).await
        };
        let reply = tokio::time::timeout(CONTROL_TIMEOUT, exchange)
            .await
            .map_err(|_| HostError::before("HostUnavailable", "hello timed out"))??
            .ok_or_else(|| HostError::before("HostUnavailable", "host closed during hello"))?;
        let hello = serde_json::from_value(response_result(reply, None)?)
            .map_err(|error| HostError::before("ProtocolViolation", error.to_string()))?;
        Ok(Self {
            reader,
            writer,
            hello,
        })
    }

    pub fn hello(&self) -> &HelloReply {
        &self.hello
    }

    /// Stops the host: it ends every Tick and Shell task, and answers once its endpoint is gone.
    pub async fn shutdown_host(self, timeout: Duration) -> Result<(), HostError> {
        self.shutdown("shutdown_host", None, timeout).await
    }

    /// Stops only after the host atomically checks it has no work to preserve.
    pub async fn shutdown_idle_host(
        self,
        own_connections: usize,
        timeout: Duration,
    ) -> Result<bool, HostError> {
        if !self
            .hello
            .capabilities
            .iter()
            .any(|capability| capability == CAPABILITY_IDLE_SHUTDOWN)
        {
            return Ok(false);
        }
        match self
            .shutdown("shutdown_idle_host", Some(own_connections), timeout)
            .await
        {
            Ok(()) => Ok(true),
            Err(error) if matches!(error.code.as_str(), "HostBusy" | "MethodNotAllowed") => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    async fn shutdown(
        mut self,
        method: &str,
        own_connections: Option<usize>,
        timeout: Duration,
    ) -> Result<(), HostError> {
        let mut params = json!({
            "epoch":self.hello.epoch,
            "operation":tickloop_ids::generate(),
        });
        if let Some(own_connections) = own_connections {
            params["own_connections"] = json!(own_connections);
        }
        let request = json!({"id":1,"method":method,"params":params});
        write_frame(&mut self.writer, &request, MANAGEMENT_FRAME_BYTES).await?;
        let reply = tokio::time::timeout(
            timeout,
            read_frame(&mut self.reader, MANAGEMENT_FRAME_BYTES),
        )
        .await
        .map_err(|_| HostError::after("OutcomeUnknown", "the Tickhost did not stop in time"))??
        .ok_or_else(|| HostError::after("OutcomeUnknown", "the Tickhost closed the connection"))?;
        response_result(reply, Some(1)).map(|_| ())
    }
}

/// The device identity of a terminal: what a launcher proves its terminal is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalDevice {
    pub device: u64,
}

impl TerminalDevice {
    #[allow(clippy::unnecessary_cast, reason = "dev_t is i32 on macOS")]
    pub fn of(descriptor: RawFd) -> std::io::Result<Self> {
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(descriptor, status.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let status = unsafe { status.assume_init() };
        if status.st_mode & libc::S_IFMT != libc::S_IFCHR {
            return Err(std::io::Error::other("descriptor is not a terminal device"));
        }
        Ok(Self {
            device: status.st_rdev as u64,
        })
    }
}

/// The binding a TickSession of a hosted instance proves in every hello and request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct HostedTerminal {
    pub endpoint: PathBuf,
    pub epoch: uuid::Uuid,
    pub terminal: uuid::Uuid,
}

/// What the host hands its launcher: where to report, under which open, with which secret.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchBinding {
    pub terminal: HostedTerminal,
    pub operation: uuid::Uuid,
    pub launch_token: String,
}

impl LaunchBinding {
    /// Reads the launch binding. A present but unreadable value is an error, never absence.
    pub fn from_environment() -> Result<Option<Self>, String> {
        match std::env::var(LAUNCH_ENV) {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(format!("{LAUNCH_ENV} is invalid: {error}")),
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|error| format!("{LAUNCH_ENV} is invalid: {error}")),
        }
    }
}

pub async fn read_frame(
    reader: &mut BufReader<OwnedReadHalf>,
    limit: usize,
) -> Result<Option<Value>, HostError> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| HostError::after("Disconnected", error.to_string()))?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(HostError::after("Disconnected", "partial frame"))
            };
        }
        let (taken, complete) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (available.len(), false),
        };
        if line.len() + taken > limit + 1 {
            return Err(HostError::before(
                "FrameTooLarge",
                format!("frame exceeds {limit} bytes"),
            ));
        }
        line.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if complete {
            line.pop();
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|error| HostError::before("InvalidFrame", error.to_string()));
        }
    }
}

pub async fn write_frame(
    writer: &mut OwnedWriteHalf,
    value: &Value,
    limit: usize,
) -> Result<(), HostError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| HostError::before("InvalidFrame", error.to_string()))?;
    if bytes.len() > limit {
        return Err(HostError::before(
            "FrameTooLarge",
            format!("frame exceeds {limit} bytes"),
        ));
    }
    bytes.push(b'\n');
    let mut written = 0;
    while written < bytes.len() {
        match writer.write(&bytes[written..]).await {
            Ok(0) => {
                return Err(HostError {
                    code: "Disconnected".into(),
                    message: "frame write returned zero bytes".into(),
                    dispatched: written != 0,
                });
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(HostError {
                    code: "Disconnected".into(),
                    message: error.to_string(),
                    dispatched: written != 0,
                });
            }
        }
    }
    Ok(())
}

/// Verifies that the endpoint is a socket owned by this user and returns that user.
fn verify_endpoint(endpoint: &Path) -> Result<u32, HostError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(endpoint)
        .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(HostError::before(
            "HostUnavailable",
            "Tickhost endpoint must be a socket owned by this user",
        ));
    }
    Ok(metadata.uid())
}

/// The user on the other end of a local socket.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> Result<u32, HostError> {
    // Linux has no getpeereid; SO_PEERCRED reports the peer's credentials.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(HostError::before(
            "HostUnavailable",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(credentials.uid)
}

/// The user on the other end of a local socket.
#[cfg(not(target_os = "linux"))]
pub fn peer_uid(stream: &UnixStream) -> Result<u32, HostError> {
    let mut uid = 0;
    let mut gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(HostError::before(
            "HostUnavailable",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(uid)
}

/// Compares secrets in time independent of where they differ.
pub fn same_secret(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// One authenticated business connection. Requests carry increasing ids; a response must echo
/// its request id, and anything else ends the connection.
pub struct HostConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    epoch: uuid::Uuid,
}

impl HostConnection {
    pub async fn connect(
        endpoint: &Path,
        role: Role,
        required: &[&str],
    ) -> Result<Self, HostError> {
        let owner = verify_endpoint(endpoint)?;
        let stream = tokio::time::timeout(CONTROL_TIMEOUT, UnixStream::connect(endpoint))
            .await
            .map_err(|_| HostError::before("HostUnavailable", "connect timed out"))?
            .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
        if peer_uid(&stream)? != owner {
            return Err(HostError::before(
                "HostUnavailable",
                "Tickhost endpoint belongs to another user",
            ));
        }
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let hello = Hello::Business {
            major: BUSINESS_MAJOR,
            minor: BUSINESS_MINOR,
            required: required.iter().map(|value| (*value).to_owned()).collect(),
            role,
        };
        let exchange = async {
            write_frame(&mut writer, &json!(hello), BUSINESS_FRAME_BYTES)
                .await
                .map_err(|error| HostError::before(&error.code, error.message))?;
            read_frame(&mut reader, BUSINESS_FRAME_BYTES).await
        };
        let reply = tokio::time::timeout(CONTROL_TIMEOUT, exchange)
            .await
            .map_err(|_| HostError::before("HostUnavailable", "hello timed out"))??
            .ok_or_else(|| HostError::before("HostUnavailable", "host closed during hello"))?;
        let reply = response_result(reply, None)
            .map_err(|error| HostError::before(&error.code, error.message))?;
        let reply: HelloReply = serde_json::from_value(reply)
            .map_err(|error| HostError::before("IncompatibleHost", error.to_string()))?;
        if reply.major != BUSINESS_MAJOR {
            return Err(HostError::before(
                "IncompatibleHost",
                format!(
                    "Tickhost business protocol {}.{} is incompatible with client {BUSINESS_MAJOR}.{BUSINESS_MINOR}",
                    reply.major, reply.minor
                ),
            ));
        }
        if let Some(missing) = required
            .iter()
            .find(|capability| !reply.capabilities.iter().any(|known| known == *capability))
        {
            return Err(HostError::before(
                "IncompatibleHost",
                format!("Tickhost lacks the required {missing} capability"),
            ));
        }
        Ok(Self {
            reader,
            writer,
            next_id: 1,
            epoch: reply.epoch,
        })
    }

    /// The process epoch of the host this connection reached.
    pub fn epoch(&self) -> uuid::Uuid {
        self.epoch
    }

    /// Sends one request and waits for its response. Failure after the request was written is
    /// dispatched: the caller must not retry a mutation, only query its receipt.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(
            &mut self.writer,
            &json!({"id":id,"method":method,"params":params}),
            BUSINESS_FRAME_BYTES,
        )
        .await?;
        let response =
            tokio::time::timeout(timeout, read_frame(&mut self.reader, BUSINESS_FRAME_BYTES))
                .await
                .map_err(|_| {
                    HostError::after("OutcomeUnknown", "Tickhost did not answer in time")
                })??
                .ok_or_else(|| {
                    HostError::after("OutcomeUnknown", "Tickhost closed the connection")
                })?;
        response_result(response, Some(id))
    }

    /// Reads the next pushed frame of a stream this connection subscribed to.
    pub async fn next_event(&mut self) -> Result<Option<Value>, HostError> {
        read_frame(&mut self.reader, BUSINESS_FRAME_BYTES).await
    }

    /// The next pushed event, refusing one larger than `limit`.
    pub async fn next_event_within(&mut self, limit: usize) -> Result<Option<Value>, HostError> {
        read_frame(&mut self.reader, limit).await
    }

    /// Passes `descriptors` to the host ahead of the next request, which they belong to.
    pub async fn send_descriptors(&self, descriptors: &[RawFd]) -> Result<(), HostError> {
        self.writer
            .as_ref()
            .async_io(tokio::io::Interest::WRITABLE, || {
                send_with_descriptors(self.writer.as_ref().as_raw_fd(), descriptors)
            })
            .await
            .map_err(|error| HostError::before("Disconnected", error.to_string()))
    }

    pub fn into_parts(self) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        (self.reader, self.writer)
    }
}

/// Outside Linux a received descriptor is close-on-exec only once marked after it arrived, so a
/// child spawned in between would inherit it. Receiving holds this lock exclusively from arrival
/// to marking; every spawn of the processes that receive descriptors shares it.
static DESCRIPTOR_ARRIVAL: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Runs `spawn`, which starts a child process, while no passed descriptor is still arriving.
pub fn spawn_child<T>(spawn: impl FnOnce() -> T) -> T {
    let _arrivals = DESCRIPTOR_ARRIVAL
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    spawn()
}

/// Receives the descriptors a peer passed ahead of its next frame. Nothing may be buffered yet:
/// the descriptors travel with the first byte after the hello reply.
pub async fn receive_descriptors(
    reader: &BufReader<OwnedReadHalf>,
    count: usize,
) -> Result<Vec<std::os::fd::OwnedFd>, HostError> {
    if !reader.buffer().is_empty() {
        return Err(HostError::before(
            "InvalidFrame",
            "descriptors must come before any other frame",
        ));
    }
    let stream = reader.get_ref().as_ref();
    let descriptors = stream
        .async_io(tokio::io::Interest::READABLE, || {
            receive_with_descriptors(stream.as_raw_fd(), count)
        })
        .await
        .map_err(|error| HostError::before("InvalidDescriptors", error.to_string()))?;
    if descriptors.len() != count {
        return Err(HostError::before(
            "InvalidDescriptors",
            format!(
                "expected {count} descriptors, received {}",
                descriptors.len()
            ),
        ));
    }
    Ok(descriptors)
}

fn descriptor_space(count: usize) -> usize {
    unsafe { libc::CMSG_SPACE((count * std::mem::size_of::<RawFd>()) as u32) as usize }
}

fn send_with_descriptors(socket: RawFd, descriptors: &[RawFd]) -> std::io::Result<()> {
    let marker = [DESCRIPTOR_MARKER];
    let mut part = libc::iovec {
        iov_base: marker.as_ptr() as *mut libc::c_void,
        iov_len: marker.len(),
    };
    let space = descriptor_space(descriptors.len());
    // Control messages are aligned like the header that leads them.
    let mut control = vec![0u64; space.div_ceil(std::mem::size_of::<u64>())];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut part;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(descriptors) as u32) as _;
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr(),
            libc::CMSG_DATA(header).cast::<RawFd>(),
            descriptors.len(),
        );
    }
    match unsafe { libc::sendmsg(socket, &message, 0) } {
        sent if sent < 0 => Err(std::io::Error::last_os_error()),
        0 => Err(std::io::ErrorKind::WriteZero.into()),
        _ => Ok(()),
    }
}

fn receive_with_descriptors(
    socket: RawFd,
    count: usize,
) -> std::io::Result<Vec<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let mut marker = [0u8];
    let mut part = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: marker.len(),
    };
    let space = descriptor_space(count);
    let mut control = vec![0u64; space.div_ceil(std::mem::size_of::<u64>())];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut part;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let _arriving = DESCRIPTOR_ARRIVAL
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let received = unsafe { libc::recvmsg(socket, &mut message, flags) };
    if received < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut descriptors = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                let length = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for index in 0..length / std::mem::size_of::<RawFd>() {
                    let descriptor = data.add(index).read_unaligned();
                    libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC);
                    descriptors.push(std::os::fd::OwnedFd::from_raw_fd(descriptor));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if received == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    if marker[0] != DESCRIPTOR_MARKER || message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(std::io::Error::other(
            "the passed descriptors were malformed or truncated",
        ));
    }
    Ok(descriptors)
}

/// Extracts a response's result, or its typed error. With an expected id, a response for any
/// other request is a protocol violation whose outcome is unknown.
pub fn response_result(response: Value, expected: Option<u64>) -> Result<Value, HostError> {
    if let Some(expected) = expected
        && response["id"].as_u64() != Some(expected)
    {
        return Err(HostError::after(
            "ProtocolViolation",
            "Tickhost answered another request",
        ));
    }
    if let Some(error) = response.get("error") {
        return Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| {
            HostError::after("ProtocolViolation", "Tickhost returned a malformed error")
        }));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| HostError::after("ProtocolViolation", "Tickhost response has no result"))
}

pub fn ok(id: &Value, result: Value) -> Value {
    json!({"id":id,"result":result})
}

pub fn error(id: &Value, error: &HostError) -> Value {
    json!({"id":id,"error":error})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_frame_over_the_readers_limit_is_refused_and_one_at_it_is_read() {
        const LIMIT: usize = 64;
        let (mut writer, reader) = tokio::net::UnixStream::pair().unwrap();
        let (reader, _) = reader.into_split();
        let mut reader = BufReader::new(reader);
        let within = format!("\"{}\"", "a".repeat(LIMIT - 2));
        let over = format!("\"{}\"", "a".repeat(LIMIT - 1));
        writer
            .write_all(format!("{within}\n{over}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut reader, LIMIT).await.unwrap(),
            Some(Value::String("a".repeat(LIMIT - 2)))
        );
        assert_eq!(
            read_frame(&mut reader, LIMIT).await.unwrap_err().code,
            "FrameTooLarge"
        );
    }

    fn identity(descriptor: RawFd) -> (u64, u64) {
        use std::os::{fd::FromRawFd, unix::fs::MetadataExt};
        let duplicate = unsafe { std::fs::File::from_raw_fd(libc::dup(descriptor)) };
        let metadata = duplicate.metadata().unwrap();
        (metadata.dev(), metadata.ino())
    }

    #[tokio::test]
    async fn passed_descriptors_arrive_ahead_of_the_frame_they_belong_to() {
        let directory = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(directory.path().join("output")).unwrap();
        let folder = std::fs::File::open(directory.path()).unwrap();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, client_writer) = client.into_split();
        let (server_reader, _) = server.into_split();
        let reader = BufReader::new(server_reader);
        let raw = client_writer.as_ref().as_raw_fd();
        tokio::task::spawn_blocking(move || {
            send_with_descriptors(raw, &[folder.as_raw_fd(), file.as_raw_fd()]).unwrap();
        })
        .await
        .unwrap();
        let mut writer = client_writer;
        write_frame(&mut writer, &json!({"method":"spawn"}), 64)
            .await
            .unwrap();
        let received = receive_descriptors(&reader, 2).await.unwrap();
        assert_eq!(
            identity(received[0].as_raw_fd()),
            identity(std::fs::File::open(directory.path()).unwrap().as_raw_fd())
        );
        assert_eq!(
            identity(received[1].as_raw_fd()),
            identity(
                std::fs::File::open(directory.path().join("output"))
                    .unwrap()
                    .as_raw_fd()
            )
        );
        let mut reader = reader;
        assert_eq!(
            read_frame(&mut reader, 64).await.unwrap(),
            Some(json!({"method":"spawn"}))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passed_descriptors_arrive_only_while_no_child_is_being_spawned() {
        let directory = tempfile::tempdir().unwrap();
        let folder = std::fs::File::open(directory.path()).unwrap();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, client_writer) = client.into_split();
        let (server_reader, _) = server.into_split();
        let (spawn_started, spawning) = std::sync::mpsc::channel();
        let (finish_spawn, spawn_may_finish) = std::sync::mpsc::channel::<()>();
        let spawner = std::thread::spawn(move || {
            spawn_child(|| {
                spawn_started.send(()).unwrap();
                spawn_may_finish.recv().unwrap();
            })
        });
        spawning.recv().unwrap();
        send_with_descriptors(client_writer.as_ref().as_raw_fd(), &[folder.as_raw_fd()]).unwrap();
        let receiving = tokio::spawn(async move {
            let reader = BufReader::new(server_reader);
            receive_descriptors(&reader, 1).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !receiving.is_finished(),
            "a descriptor arrived while a child was being spawned"
        );
        finish_spawn.send(()).unwrap();
        spawner.join().unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), receiving)
            .await
            .expect("the descriptor arrives once the spawn is done")
            .unwrap()
            .unwrap();
        assert_eq!(received.len(), 1);
        assert_ne!(
            unsafe { libc::fcntl(received[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }

    #[tokio::test]
    async fn a_frame_in_place_of_the_descriptors_is_refused() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, mut writer) = client.into_split();
        let (server_reader, _) = server.into_split();
        let reader = BufReader::new(server_reader);
        write_frame(&mut writer, &json!({"method":"spawn"}), 64)
            .await
            .unwrap();
        assert_eq!(
            receive_descriptors(&reader, 2).await.unwrap_err().code,
            "InvalidDescriptors"
        );
    }
}
