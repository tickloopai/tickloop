use std::{
    collections::BTreeMap,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

use crate::host_error::HostError;

pub const HOST_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_RESPONSE_LIMIT: u64 = 8 * 1024 * 1024;
const PANE_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5);
/// The Pane metadata token through which a view Pane names the Tick it shows, so its Worker
/// recognises it, also after a restart.
pub const VIEW_TOKEN: &str = "tickloop_view";
const METADATA_SOURCE: &str = "tickloop";
/// How long a reported view lasts unless reported again, so a view that ended without clearing
/// it never names a Tick for long.
pub const VIEW_TOKEN_TTL: Duration = Duration::from_secs(30);
/// How often a view reports itself again while it shows its Tick.
pub const VIEW_TOKEN_RENEWAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct HerdrContext {
    pub socket: PathBuf,
    pub caller_pane: String,
}

impl HerdrContext {
    pub fn without_view(socket: PathBuf) -> Self {
        Self {
            socket,
            caller_pane: String::new(),
        }
    }

    pub fn from_environment() -> Result<Option<Self>, String> {
        if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
            return Ok(None);
        }
        let socket = std::env::var_os("HERDR_SOCKET_PATH")
            .filter(|value| !value.is_empty())
            .ok_or("HERDR_SOCKET_PATH is required inside Herdr")?;
        let caller_pane = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or("HERDR_PANE_ID is required inside Herdr")?;
        Ok(Some(Self {
            socket: socket.into(),
            caller_pane,
        }))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    /// Display metadata programs in the Pane reported, by token name.
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

impl Pane {
    /// The Tick this Pane shows a view of, as the view reported it.
    pub fn view_of(&self) -> Option<&str> {
        self.tokens.get(VIEW_TOKEN).map(String::as_str)
    }
}

/// Reports on Pane `pane_id` the Tick `instance` it shows a view of, for [`VIEW_TOKEN_TTL`], or
/// with `None` that it shows none.
pub async fn report_view(
    host: &dyn HerdrPort,
    pane_id: &str,
    instance: Option<&str>,
) -> Result<(), HostError> {
    let metadata = match instance {
        Some(instance) => json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": {VIEW_TOKEN: instance},
            "ttl_ms": VIEW_TOKEN_TTL.as_millis() as u64,
        }),
        None => json!({"pane_id":pane_id,"source":METADATA_SOURCE,"tokens":{VIEW_TOKEN: null}}),
    };
    host.request("pane.report_metadata", metadata, CancellationToken::new())
        .await
        .map(drop)
}

/// Why a recorded Herdr Pane can no longer be confirmed as the same Pane.
#[derive(Debug, thiserror::Error)]
pub enum PaneIdentityError {
    /// The endpoint is not this user's socket, or Herdr refused the lookup.
    #[error("{0}")]
    Denied(String),
    /// The Herdr instance or the Pane's terminal is not the one recorded.
    #[error("{0}")]
    Replaced(&'static str),
    #[error("Herdr did not respond")]
    Unresponsive,
}

/// The Herdr instance serving a socket: the socket file's device and inode. A restarted Herdr
/// binds a new socket file, so a recorded instance no longer matches it.
pub fn endpoint_instance(socket: &std::fs::Metadata) -> String {
    format!("herdr:{}:{}", socket.dev(), socket.ino())
}

/// `pane_id` as the Herdr at `socket` reports it now. Before and after the lookup the endpoint
/// must be a socket owned by this user and still the recorded `instance`; the Pane must still
/// show `terminal_id`.
pub async fn confirm_pane(
    socket: &Path,
    instance: &str,
    pane_id: &str,
    terminal_id: &str,
) -> Result<Pane, PaneIdentityError> {
    verify_endpoint(socket, instance)?;
    let host = SocketHerdr::new(HerdrContext::without_view(socket.to_path_buf()));
    let pane = tokio::time::timeout(PANE_CONFIRMATION_TIMEOUT, current_pane(&host, pane_id))
        .await
        .map_err(|_| PaneIdentityError::Unresponsive)?
        .map_err(|error| PaneIdentityError::Denied(error.to_string()))?;
    verify_endpoint(socket, instance)?;
    if pane.terminal_id != terminal_id {
        return Err(PaneIdentityError::Replaced(
            "Herdr Pane or terminal was replaced",
        ));
    }
    Ok(pane)
}

fn verify_endpoint(socket: &Path, instance: &str) -> Result<(), PaneIdentityError> {
    let metadata = std::fs::symlink_metadata(socket).map_err(|error| {
        PaneIdentityError::Denied(format!("cannot inspect Herdr endpoint: {error}"))
    })?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(PaneIdentityError::Denied(
            "Herdr endpoint must be a socket owned by this user".into(),
        ));
    }
    if endpoint_instance(&metadata) != instance {
        return Err(PaneIdentityError::Replaced("Herdr endpoint was replaced"));
    }
    Ok(())
}

/// The Pane `caller_pane` names now, as Herdr reports it to a program running in that Pane.
pub async fn current_pane(host: &dyn HerdrPort, caller_pane: &str) -> Result<Pane, HostError> {
    let response = host
        .request(
            "pane.current",
            json!({"caller_pane_id":caller_pane}),
            CancellationToken::new(),
        )
        .await?;
    serde_json::from_value(response["pane"].clone()).map_err(|error| {
        HostError::before(
            "ProtocolViolation",
            format!("invalid Herdr response: {error}"),
        )
    })
}

#[async_trait]
pub trait HerdrPort: Send + Sync {
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, HostError>;
}

pub struct SocketHerdr {
    context: HerdrContext,
}

impl SocketHerdr {
    pub fn new(context: HerdrContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl HerdrPort for SocketHerdr {
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        let mut dispatched = false;
        let request_id = tickloop_ids::generate().to_string();
        let work = async {
            let mut stream = UnixStream::connect(&self.context.socket)
                .await
                .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
            let mut bytes =
                serde_json::to_vec(&json!({"id":request_id,"method":method,"params":params}))
                    .map_err(|error| HostError::before("InvalidArguments", error.to_string()))?;
            bytes.push(b'\n');
            if cancel.is_cancelled() {
                return Err(HostError::before("Cancelled", "request was not sent"));
            }
            // A partial write can already reach the host, so cancellation after this point is uncertain.
            dispatched = true;
            stream.write_all(&bytes).await.map_err(|error| HostError {
                code: "OutcomeUnknown".into(),
                message: error.to_string(),
                dispatched: true,
            })?;
            let mut frame = Vec::new();
            BufReader::new(stream)
                .take(HOST_RESPONSE_LIMIT + 1)
                .read_until(b'\n', &mut frame)
                .await
                .map_err(|error| HostError {
                    code: "HostUnavailable".into(),
                    message: error.to_string(),
                    dispatched: true,
                })?;
            if frame.len() as u64 > HOST_RESPONSE_LIMIT || !frame.ends_with(b"\n") {
                return Err(HostError {
                    code: "ProtocolViolation".into(),
                    message: "Herdr returned an incomplete or oversized response".into(),
                    dispatched: true,
                });
            }
            let response: Value = serde_json::from_slice(&frame).map_err(|error| HostError {
                code: "ProtocolViolation".into(),
                message: error.to_string(),
                dispatched: true,
            })?;
            if response["id"] != request_id {
                return Err(HostError {
                    code: "ProtocolViolation".into(),
                    message: "Herdr response identity does not match the request".into(),
                    dispatched: true,
                });
            }
            if let Some(error) = response.get("error") {
                return Err(HostError {
                    code: error["code"].as_str().unwrap_or("HostError").into(),
                    message: error["message"]
                        .as_str()
                        .unwrap_or("Herdr rejected request")
                        .into(),
                    dispatched: false,
                });
            }
            response.get("result").cloned().ok_or_else(|| HostError {
                code: "ProtocolViolation".into(),
                message: "Herdr response has no result".into(),
                dispatched: true,
            })
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => None,
            result = tokio::time::timeout(HOST_TIMEOUT, work) => Some(result),
        };
        match result {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(HostError {
                code: if dispatched {
                    "OutcomeUnknown"
                } else {
                    "Timeout"
                }
                .into(),
                message: "Herdr request exceeded 10 seconds".into(),
                dispatched,
            }),
            None => Err(HostError {
                code: if dispatched {
                    "OutcomeUnknown"
                } else {
                    "Cancelled"
                }
                .into(),
                message: "Herdr request was cancelled".into(),
                dispatched,
            }),
        }
    }
}
