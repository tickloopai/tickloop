use super::{CONNECT_TIMEOUT, Entry, Handler, redact};
use crate::mcp_http::{McpHttpError, initialize_failure, nested_http_error};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{ClientJsonRpcMessage, ClientRequest, ErrorCode, ProtocolVersion, RequestId},
    service::{ClientInitializeError, RunningService},
    transport::{Transport, streamable_http_client::StreamableHttpError},
};
use serde_json::Value;
use std::{
    io,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

#[derive(Clone, Default)]
pub(crate) struct ProbeState {
    observation: Arc<Mutex<ProbeObservation>>,
    cancellation: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct ProbeObservation {
    request_id: Option<RequestId>,
    modern_version_rejected: bool,
    http_status: Option<u16>,
    deadline: Option<tokio::time::Instant>,
    budget: Duration,
}

impl ProbeState {
    fn start(&self, budget: Duration) -> tokio::time::Instant {
        let deadline = tokio::time::Instant::now() + budget;
        let mut observation = self.observation.lock().unwrap();
        observation.deadline = Some(deadline);
        observation.budget = budget;
        deadline
    }

    pub(crate) fn limit(&self) -> Option<(tokio::time::Instant, Duration)> {
        let observation = self.observation.lock().unwrap();
        observation
            .deadline
            .map(|deadline| (deadline, observation.budget))
    }

    pub(crate) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub(crate) fn sending(&self, message: &ClientJsonRpcMessage) {
        if let ClientJsonRpcMessage::Request(request) = message {
            match &request.request {
                ClientRequest::DiscoverRequest(_) => {
                    let mut observation = self.observation.lock().unwrap();
                    observation.request_id = Some(request.id.clone());
                    observation.modern_version_rejected = false;
                    observation.http_status = None;
                }
                ClientRequest::InitializeRequest(_) => self.finish(),
                _ => {}
            }
        }
    }

    pub(crate) fn finish(&self) {
        let mut observation = self.observation.lock().unwrap();
        observation.request_id = None;
        observation.deadline = None;
    }

    pub(crate) fn http_error_response(&self, status: u16) {
        self.observation.lock().unwrap().http_status = Some(status);
    }

    // Keep only routing facts across redaction, never an arbitrary server string.
    pub(crate) fn prepare_response(&self, message: &mut Value) -> Vec<(String, Value)> {
        let mut observation = self.observation.lock().unwrap();
        let Some(id) = &observation.request_id else {
            return Vec::new();
        };
        let mut routing = Vec::new();
        if message.get("error").is_some() {
            message["id"] = serde_json::to_value(id).expect("request id serializes");
            let unsupported = message["error"]["code"].as_i64()
                == Some(i64::from(ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0));
            observation.modern_version_rejected = unsupported
                && message["error"]["data"]["supported"]
                    .as_array()
                    .is_some_and(|versions| {
                        !versions.is_empty()
                            && versions.iter().all(Value::is_string)
                            && versions
                                .iter()
                                .filter_map(Value::as_str)
                                .any(|version| version >= ProtocolVersion::V_2026_07_28.as_str())
                    });
            if unsupported {
                routing.push(("/error/code".into(), message["error"]["code"].clone()));
            }
        } else if message.get("result").is_some() {
            observation.modern_version_rejected = false;
        }
        for pointer in ["/result/supportedVersions", "/error/data/supported"] {
            if let Some(versions) = message.pointer(pointer).and_then(Value::as_array) {
                for (index, version) in versions.iter().enumerate() {
                    if ProtocolVersion::KNOWN_VERSIONS
                        .iter()
                        .any(|known| Some(known.as_str()) == version.as_str())
                    {
                        routing.push((format!("{pointer}/{index}"), version.clone()));
                    }
                }
            }
        }
        routing
    }

    fn legacy(&self, error: &ClientInitializeError, http: bool) -> bool {
        match error {
            ClientInitializeError::NoCompatibleProtocolVersion { .. } => {
                !self.observation.lock().unwrap().modern_version_rejected
            }
            ClientInitializeError::JsonRpcError(_) => {
                !self.observation.lock().unwrap().modern_version_rejected
            }
            ClientInitializeError::ExpectedInitResponse(_)
            | ClientInitializeError::ExpectedInitResult(_)
            | ClientInitializeError::ConflictInitResponseId(..)
            | ClientInitializeError::UncorrelatedErrorResponse { .. } => true,
            ClientInitializeError::ConnectionClosed(_) => !http,
            ClientInitializeError::TransportError { error, .. } => {
                if let Some(error) = error
                    .error
                    .downcast_ref::<StreamableHttpError<McpHttpError>>()
                {
                    match error {
                        StreamableHttpError::Client(McpHttpError::Status(status)) => {
                            !matches!(status.as_u16(), 401 | 403)
                        }
                        StreamableHttpError::Sse(error) => nested_http_error(error).is_none(),
                        StreamableHttpError::UnexpectedServerResponse(_)
                        | StreamableHttpError::UnexpectedContentType(_)
                        | StreamableHttpError::UnexpectedEndOfStream
                        | StreamableHttpError::Deserialize(_) => true,
                        _ => false,
                    }
                } else if !http && let Some(error) = error.error.downcast_ref::<io::Error>() {
                    matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                    )
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn failure(&self, entry: &Entry, error: &ClientInitializeError) -> String {
        let cause = initialize_failure(error, &|text| redact(entry, text));
        match self.observation.lock().unwrap().http_status {
            Some(status) => format!("MCP returned HTTP {status}: {cause}"),
            None => cause,
        }
    }
}

pub(super) async fn connect<T: Transport<RoleClient> + 'static>(
    entry: &Arc<Entry>,
    handler: Handler,
    probe: &ProbeState,
    http: bool,
    mut transport: impl FnMut() -> Result<T, String>,
) -> Result<RunningService<RoleClient, Handler>, String> {
    let _probe_guard = probe.cancellation.clone().drop_guard();
    let discover_failure = if entry.legacy.load(Ordering::SeqCst) {
        None
    } else {
        let budget = Duration::from_millis(entry.config.timeout_ms).min(CONNECT_TIMEOUT) / 3;
        let deadline = probe.start(budget);
        let result = tokio::time::timeout_at(
            deadline,
            handler.clone().serve_with_lifecycle(
                transport()?,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                },
            ),
        )
        .await;
        let cause = match result {
            Ok(Ok(service)) => {
                probe.finish();
                return Ok(service);
            }
            Ok(Err(error)) => {
                let cause = probe.failure(entry, &error);
                if !probe.legacy(&error, http) {
                    return Err(cause);
                }
                cause
            }
            Err(_) => {
                let cause = format!(
                    "MCP discover probe timed out after {}ms",
                    budget.as_millis()
                );
                if http {
                    return Err(cause);
                }
                cause
            }
        };
        entry.legacy.store(true, Ordering::SeqCst);
        Some(cause)
    };
    probe.finish();
    let transport = transport().map_err(|cause| match &discover_failure {
        Some(discover) => format!("discover failed: {discover}; legacy initialize failed: {cause}"),
        None => cause,
    })?;
    handler
        .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
        .await
        .map_err(|error| {
            let cause = initialize_failure(&error, &|text| redact(entry, text));
            match discover_failure {
                Some(discover) => {
                    format!("discover failed: {discover}; legacy initialize failed: {cause}")
                }
                None => cause,
            }
        })
}
