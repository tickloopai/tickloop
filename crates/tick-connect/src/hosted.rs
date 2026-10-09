//! The Tick host wire contract shared by the host service, its Agent owner client, data clients
//! and the hosted launcher. Frames are one JSON object per line on a private Unix socket. The
//! first frame names the channel: the frozen management channel only says hello and shuts the
//! host down; the business channel negotiates its minor version and capabilities and then
//! serves exactly one authenticated role.

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
use crate::instance::InstanceRef;

pub const MANAGEMENT_MAJOR: u16 = 1;
pub const MANAGEMENT_MINOR: u16 = 0;
pub const MANAGEMENT_FRAME_BYTES: usize = 64 * 1024;
pub const BUSINESS_MAJOR: u16 = 1;
pub const BUSINESS_MINOR: u16 = 0;
/// A complete business frame, large enough for one legal 8 MiB Tick Tool response.
pub const BUSINESS_FRAME_BYTES: usize = 8 * 1024 * 1024 + 64 * 1024;
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a data client waits for a call's answer: the Tick's whole budget plus transport.
pub const CALL_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(CALL_TIMEOUT.as_secs() + CONTROL_TIMEOUT.as_secs());
/// One input chunk of an attached view; larger input is sent as several chunks in order.
pub const ATTACH_INPUT_BYTES: usize = 4 * 1024;
/// An attached view's request frame: one input chunk, a resize or detach.
pub const ATTACH_REQUEST_FRAME_BYTES: usize = 64 * 1024;
/// The launch binding a hosted launcher reads from its environment.
pub const LAUNCH_ENV: &str = "TICKLOOP_TICK_HOST_LAUNCH";

pub const CAPABILITY_LIFECYCLE: &str = "lifecycle";
pub const CAPABILITY_DATA: &str = "data";
pub const CAPABILITY_LAUNCH: &str = "launch";
pub const BUSINESS_CAPABILITIES: [&str; 3] =
    [CAPABILITY_LIFECYCLE, CAPABILITY_DATA, CAPABILITY_LAUNCH];

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
    /// The Agent owner daemon: the only writer of opens, closes and ownership.
    Controller {
        controller_key: String,
        owner_epoch: uuid::Uuid,
    },
    /// A reader of one instance, holding the data token issued when it opened.
    Instance {
        instance: InstanceRef,
        data_token: String,
    },
    /// The launcher of one pending open, holding that open's one-time launch token.
    Launcher {
        operation: uuid::Uuid,
        launch_token: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HelloReply {
    pub epoch: uuid::Uuid,
    pub pid: u32,
    pub major: u16,
    pub minor: u16,
    pub capabilities: Vec<String>,
}

/// The device identity of a terminal: what a launcher proves its terminal is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalDevice {
    pub device: u64,
}

impl TerminalDevice {
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
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| HostError::after("Disconnected", error.to_string()))
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
            "Tick host endpoint must be a socket owned by this user",
        ));
    }
    Ok(metadata.uid())
}

/// The user on the other end of a local socket.
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
                "Tick host endpoint belongs to another user",
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
                    "Tick host business protocol {}.{} is incompatible with client {BUSINESS_MAJOR}.{BUSINESS_MINOR}",
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
                format!("Tick host lacks the required {missing} capability"),
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
        .await
        .map_err(|error| HostError::before(&error.code, error.message))?;
        let response =
            tokio::time::timeout(timeout, read_frame(&mut self.reader, BUSINESS_FRAME_BYTES))
                .await
                .map_err(|_| {
                    HostError::after("OutcomeUnknown", "Tick host did not answer in time")
                })??
                .ok_or_else(|| {
                    HostError::after("OutcomeUnknown", "Tick host closed the connection")
                })?;
        response_result(response, Some(id))
    }

    /// Reads the next pushed frame of a stream this connection subscribed to.
    pub async fn next_event(&mut self) -> Result<Option<Value>, HostError> {
        read_frame(&mut self.reader, BUSINESS_FRAME_BYTES).await
    }

    pub fn into_parts(self) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        (self.reader, self.writer)
    }
}

/// Extracts a response's result, or its typed error. With an expected id, a response for any
/// other request is a protocol violation whose outcome is unknown.
pub fn response_result(response: Value, expected: Option<u64>) -> Result<Value, HostError> {
    if let Some(expected) = expected
        && response["id"].as_u64() != Some(expected)
    {
        return Err(HostError::after(
            "ProtocolViolation",
            "Tick host answered another request",
        ));
    }
    if let Some(error) = response.get("error") {
        return Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| {
            HostError::after("ProtocolViolation", "Tick host returned a malformed error")
        }));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| HostError::after("ProtocolViolation", "Tick host response has no result"))
}

pub fn ok(id: &Value, result: Value) -> Value {
    json!({"id":id,"result":result})
}

pub fn error(id: &Value, error: &HostError) -> Value {
    json!({"id":id,"error":error})
}
