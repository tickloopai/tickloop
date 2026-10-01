use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

pub const HOST_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_RESPONSE_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct HerdrContext {
    pub socket: PathBuf,
    pub caller_pane: String,
}

impl HerdrContext {
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
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct HostError {
    pub code: String,
    pub message: String,
    pub dispatched: bool,
}

impl HostError {
    pub fn before(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            dispatched: false,
        }
    }
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
        let request_id = uuid::Uuid::new_v4().to_string();
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

pub fn process_birth(pid: u32) -> Result<String, HostError> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        // proc_pidinfo initializes the entire fixed-size structure only on a full-size success.
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size as i32,
            )
        };
        if count as usize != size {
            return Err(HostError::before(
                "IdentityUncertain",
                format!("cannot verify process {pid} birth"),
            ));
        }
        let info = unsafe { info.assume_init() };
        Ok(format!(
            "{pid}:{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|error| HostError::before("IdentityUncertain", error.to_string()))?;
        let (_, fields) = stat
            .rsplit_once(')')
            .ok_or_else(|| HostError::before("IdentityUncertain", "invalid process stat"))?;
        let birth = fields
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| HostError::before("IdentityUncertain", "process birth unavailable"))?;
        Ok(format!("{pid}:{birth}"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(HostError::before(
            "IdentityUncertain",
            format!("process {pid} birth verification is unsupported on this OS"),
        ))
    }
}
