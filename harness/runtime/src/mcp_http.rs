use std::{
    borrow::Cow,
    collections::HashMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue};
use reqwest::{Client, Method, StatusCode, Url, header, redirect::Policy};
use rmcp::{
    ServiceError,
    model::{
        ClientJsonRpcMessage, ClientRequest, CustomResult, ErrorCode, JsonRpcResponse,
        ServerJsonRpcMessage, ServerResult,
    },
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::{Sse, SseStream};

use tickloop_harness_base::network::{HttpPolicy, build_http_client};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("Tickloop-Agent/", env!("CARGO_PKG_VERSION"), " MCP/1");
pub(crate) const REQUEST_TIMED_OUT: &str = "MCP request timed out";
const INVALID_RESPONSE: &str = "invalid MCP response";
const CONNECTION_FAILED: &str = "MCP connection failed";
const RESOURCE_NOT_FOUND: &str = "MCP resource not found";
const MAX_URI_CHARS: usize = 200;

/// Why the HTTP exchange under an MCP message failed. Every variant shows only fixed text, so
/// neither the bearer nor anything the server sent can surface through it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum McpHttpError {
    #[error("MCP connection failed")]
    Connection,
    #[error("MCP request timed out")]
    TimedOut,
    #[error("MCP returned HTTP {0}")]
    Status(StatusCode),
    #[error("MCP credential unavailable")]
    CredentialUnavailable,
}

impl From<reqwest::Error> for McpHttpError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::TimedOut
        } else {
            Self::Connection
        }
    }
}

/// Short English for a failed MCP request, shown to the model and the user. Nothing the server
/// sent appears in it: a JSON-RPC error keeps only its code, and a missing resource names the URI
/// the caller asked for.
pub(crate) fn request_failure(error: &ServiceError, resource_uri: Option<&str>) -> String {
    match error {
        ServiceError::McpError(error) if error.code == ErrorCode::RESOURCE_NOT_FOUND => {
            match resource_uri.map(bounded_line).filter(|uri| !uri.is_empty()) {
                Some(uri) => format!("{RESOURCE_NOT_FOUND}: {uri}"),
                None => RESOURCE_NOT_FOUND.into(),
            }
        }
        ServiceError::McpError(error) => format!("MCP server error {}", error.code.0),
        ServiceError::TransportSend(error) => error
            .error
            .downcast_ref::<StreamableHttpError<McpHttpError>>()
            .map_or_else(|| CONNECTION_FAILED.into(), transport_failure),
        ServiceError::Timeout { .. } => REQUEST_TIMED_OUT.into(),
        ServiceError::TransportClosed => CONNECTION_FAILED.into(),
        ServiceError::Cancelled { .. } => "MCP request cancelled".into(),
        _ => INVALID_RESPONSE.into(),
    }
}

pub(crate) fn session_expired(error: &ServiceError) -> bool {
    matches!(error, ServiceError::TransportSend(error) if matches!(error.error.downcast_ref::<StreamableHttpError<McpHttpError>>(), Some(StreamableHttpError::Client(McpHttpError::Status(StatusCode::NOT_FOUND)))))
}

fn transport_failure(error: &StreamableHttpError<McpHttpError>) -> String {
    match error {
        StreamableHttpError::Client(McpHttpError::Status(StatusCode::UNAUTHORIZED)) => {
            "MCP authorization rejected (HTTP 401)".into()
        }
        StreamableHttpError::Client(McpHttpError::Status(StatusCode::FORBIDDEN)) => {
            "MCP access forbidden (HTTP 403)".into()
        }
        StreamableHttpError::Client(error) => error.to_string(),
        StreamableHttpError::UnexpectedServerResponse(_)
        | StreamableHttpError::UnexpectedContentType(_)
        | StreamableHttpError::Deserialize(_) => INVALID_RESPONSE.into(),
        _ => CONNECTION_FAILED.into(),
    }
}

/// One bounded line: control and invisible formatting characters become spaces and whitespace
/// runs collapse.
fn bounded_line(text: &str) -> String {
    let line = text
        .chars()
        .map(|character| {
            if character.is_control() || is_invisible_format(character) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() <= MAX_URI_CHARS {
        return line;
    }
    let mut bounded = line.chars().take(MAX_URI_CHARS).collect::<String>();
    bounded.push('…');
    bounded
}

/// Zero-width and bidirectional controls that could hide or reorder text shown to the user.
fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

#[derive(Clone, Copy)]
pub(crate) enum ResponsePart {
    Data,
    ToolDirectory,
}
type ResponseRedactor = Arc<dyn Fn(&mut serde_json::Value, ResponsePart) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct McpHttpClient {
    client: Client,
    endpoint: Url,
    token: Arc<str>,
    unauthorized: Arc<AtomicBool>,
    insufficient_scope: Arc<AtomicBool>,
    artifact: Option<tickloop_harness_artifacts::ArtifactClient>,
    redactor: Option<ResponseRedactor>,
    headers: HashMap<HeaderName, HeaderValue>,
}

impl McpHttpClient {
    pub(crate) fn new(endpoint: Url, token: String) -> Result<Self, String> {
        let mut policy = HttpPolicy::new(Policy::none());
        policy.timeout = Some(REQUEST_TIMEOUT);
        policy.connect_timeout = Some(Duration::from_secs(5));
        let (client, _) =
            build_http_client(&endpoint, policy).map_err(|_| "MCP HTTP client unavailable")?;
        Ok(Self {
            client,
            endpoint,
            token: token.into(),
            unauthorized: Arc::new(AtomicBool::new(false)),
            insufficient_scope: Arc::new(AtomicBool::new(false)),
            artifact: None,
            redactor: None,
            headers: HashMap::new(),
        })
    }

    pub(crate) fn for_server(
        endpoint: Url,
        headers: &std::collections::BTreeMap<String, String>,
        token: Option<String>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let local = endpoint.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && local)
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err("MCP endpoint requires HTTPS or loopback HTTP".into());
        }
        let mut http = Self::new(endpoint.clone(), token.unwrap_or_default())?;
        let mut policy = HttpPolicy::new(Policy::none());
        policy.timeout = Some(timeout);
        policy.connect_timeout = Some(Duration::from_secs(5));
        http.client = build_http_client(&endpoint, policy)
            .map_err(|_| "MCP HTTP client unavailable")?
            .0;
        for (name, value) in headers {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid MCP header name")?;
            if matches!(
                name.as_str(),
                "host"
                    | "accept"
                    | "content-type"
                    | "mcp-session-id"
                    | "last-event-id"
                    | "content-length"
                    | "transfer-encoding"
            ) {
                return Err("reserved MCP header".into());
            }
            let value = HeaderValue::from_str(value).map_err(|_| "invalid MCP header value")?;
            http.headers.insert(name, value);
        }
        Ok(http)
    }

    pub(crate) fn with_redactor(mut self, redactor: ResponseRedactor) -> Self {
        self.redactor = Some(redactor);
        self
    }

    pub(crate) fn with_artifact_credentials(
        mut self,
        artifact: tickloop_harness_artifacts::ArtifactClient,
    ) -> Self {
        self.artifact = artifact.is_host_managed().then_some(artifact);
        self
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StreamableHttpError<McpHttpError>> {
        let Some(artifact) = &self.artifact else {
            return request
                .send()
                .await
                .map_err(|error| StreamableHttpError::Client(error.into()));
        };
        let Some(credential) = artifact.access_snapshot().await.ok().flatten() else {
            return Err(StreamableHttpError::Client(
                McpHttpError::CredentialUnavailable,
            ));
        };
        let failure = match artifact.send_authenticated(request, &credential).await {
            Ok(response) => return Ok(response),
            Err(tickloop_harness_artifacts::ArtifactError::AccessDenied) => {
                self.unauthorized.store(true, Ordering::SeqCst);
                McpHttpError::Status(StatusCode::UNAUTHORIZED)
            }
            Err(tickloop_harness_artifacts::ArtifactError::Connection) => McpHttpError::Connection,
            Err(tickloop_harness_artifacts::ArtifactError::TimedOut) => McpHttpError::TimedOut,
            Err(_) => McpHttpError::CredentialUnavailable,
        };
        Err(StreamableHttpError::Client(failure))
    }

    pub(crate) fn unauthorized(&self) -> bool {
        self.unauthorized.load(Ordering::SeqCst)
    }

    fn request(
        &self,
        method: Method,
        uri: &str,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
    ) -> Result<reqwest::RequestBuilder, StreamableHttpError<McpHttpError>> {
        let url = Url::parse(uri).map_err(|_| unexpected("invalid MCP endpoint"))?;
        if url != self.endpoint || auth.as_deref().unwrap_or("") != self.token.as_ref() {
            return Err(unexpected("MCP endpoint or authorization changed"));
        }
        let mut request = self
            .client
            .request(method, url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "application/json, text/event-stream");
        if !self.token.is_empty() {
            request = request.bearer_auth(self.token.as_ref());
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        for (name, value) in custom {
            if matches!(
                name.as_str(),
                "authorization"
                    | "accept"
                    | "content-type"
                    | "host"
                    | "mcp-session-id"
                    | "last-event-id"
            ) {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        Ok(request)
    }

    pub(crate) fn insufficient_scope(&self) -> bool {
        self.insufficient_scope.load(Ordering::SeqCst)
    }

    fn verify_status(
        &self,
        response: &reqwest::Response,
    ) -> Result<(), StreamableHttpError<McpHttpError>> {
        let status = response.status();
        let insufficient_scope = status == StatusCode::FORBIDDEN
            && response
                .headers()
                .get_all(header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|value| {
                    rmcp::transport::auth::WWWAuthenticateParams::parse(value, &self.endpoint)
                        .error
                        .as_deref()
                        == Some("insufficient_scope")
                });
        if insufficient_scope {
            self.insufficient_scope.store(true, Ordering::SeqCst);
        }
        if status == StatusCode::UNAUTHORIZED || insufficient_scope {
            self.unauthorized.store(true, Ordering::SeqCst);
        }
        if !status.is_success() {
            return Err(StreamableHttpError::Client(McpHttpError::Status(status)));
        }
        Ok(())
    }
}

fn unexpected(message: impl Into<Cow<'static, str>>) -> StreamableHttpError<McpHttpError> {
    StreamableHttpError::UnexpectedServerResponse(message.into())
}

async fn json_bytes(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, StreamableHttpError<McpHttpError>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| StreamableHttpError::Client(error.into()))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_JSON_BYTES {
            return Err(unexpected("MCP JSON response too large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn sse_stream(
    response: reqwest::Response,
    max_event_bytes: usize,
    redactor: Option<ResponseRedactor>,
    directory_request: Option<rmcp::model::RequestId>,
) -> BoxStream<'static, Result<Sse, SseError>> {
    let mut event_bytes = 0_usize;
    let mut line_bytes = 0_usize;
    let stream = response.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(|_| io::Error::other("MCP event stream failed"))?;
        for byte in &chunk {
            event_bytes = event_bytes.saturating_add(1);
            if *byte == b'\n' {
                if line_bytes == 0 {
                    event_bytes = 0;
                }
                line_bytes = 0;
            } else if *byte != b'\r' {
                line_bytes += 1;
            }
            if event_bytes > max_event_bytes {
                return Err(io::Error::other("MCP SSE event too large"));
            }
        }
        Ok(chunk)
    });
    SseStream::from_bytes_stream(stream)
        .map(move |event| {
            event.and_then(|mut event| {
                if let Some(redact) = &redactor {
                    if let Some(data) = event.data.take() {
                        event.data = Some(
                            redact_message(&data, redact, directory_request.as_ref()).map_err(
                                |_| {
                                    SseError::Body(Box::new(io::Error::other(
                                        "invalid MCP event response",
                                    )))
                                },
                            )?,
                        );
                    }
                    for value in [&mut event.id, &mut event.event].into_iter().flatten() {
                        let mut text = serde_json::Value::String(std::mem::take(value));
                        redact(&mut text, ResponsePart::Data);
                        *value = text.as_str().unwrap_or_default().to_owned();
                    }
                }
                Ok(event)
            })
        })
        .boxed()
}

fn redact_message(
    text: &str,
    redact: &ResponseRedactor,
    directory_request: Option<&rmcp::model::RequestId>,
) -> Result<String, serde_json::Error> {
    let mut message: serde_json::Value = serde_json::from_str(text)?;
    let directory_response = message.get("method").is_none()
        && directory_request
            .is_some_and(|id| serde_json::to_value(id).ok().as_ref() == message.get("id"));
    if let Some(id) = message.get_mut("id").filter(|id| id.is_string()) {
        redact(id, ResponsePart::Data);
    }
    for field in ["params", "result", "error"] {
        if let Some(payload) = message.get_mut(field) {
            if field == "result"
                && let Some(tools) = payload
                    .get_mut("tools")
                    .and_then(serde_json::Value::as_array_mut)
            {
                tools.retain(|tool| {
                    let Some(name) = tool.get("name") else {
                        return true;
                    };
                    let mut display = name.clone();
                    redact(&mut display, ResponsePart::Data);
                    display == *name
                });
            }
            // The protocol version and envelope are routing facts, not server display text.
            let protocol_version = (field == "result")
                .then(|| payload.get("protocolVersion").cloned())
                .flatten()
                .filter(|version| matches!(version.as_str(), Some("2026-07-28" | "2025-11-25")));
            let part = if field == "result" && directory_response {
                ResponsePart::ToolDirectory
            } else {
                ResponsePart::Data
            };
            redact(payload, part);
            if let Some(version) = protocol_version
                && let Some(fields) = payload.as_object_mut()
            {
                fields.insert("protocolVersion".into(), version);
            }
        }
    }
    // Reject undecodable messages here so SDK diagnostics cannot echo their raw fields.
    let decoded = serde_json::from_value::<ServerJsonRpcMessage>(message.clone())?;
    let custom_method = match decoded {
        ServerJsonRpcMessage::Notification(notification) => matches!(
            notification.notification,
            rmcp::model::ServerNotification::CustomNotification(_)
        ),
        ServerJsonRpcMessage::Request(request) => matches!(
            request.request,
            rmcp::model::ServerRequest::CustomRequest(_)
        ),
        _ => false,
    };
    if custom_method && let Some(method) = message.get_mut("method") {
        redact(method, ResponsePart::Data);
    }
    serde_json::to_string(&message)
}

impl StreamableHttpClient for McpHttpClient {
    type Error = McpHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            MAX_JSON_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let directory_request = match &message {
            ClientJsonRpcMessage::Request(request)
                if matches!(request.request, ClientRequest::ListToolsRequest(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        };
        let preserve_custom_result = matches!(
            &message,
            ClientJsonRpcMessage::Request(request)
                if matches!(&request.request, ClientRequest::CustomRequest(_))
        );
        let mut request = self.request(Method::POST, &uri, auth_header, custom_headers)?;
        if let Some(session_id) = session_id {
            request = request.header("Mcp-Session-Id", session_id.as_ref());
        }
        let response = self.send(request.json(&message)).await?;
        self.verify_status(&response)?;
        if matches!(
            response.status(),
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let (Some(id), Some(redact)) = (&session_id, &self.redactor) {
            let mut redacted = serde_json::Value::String(id.clone());
            redact(&mut redacted, ResponsePart::Data);
            if redacted.as_str() != Some(id.as_str()) {
                return Err(unexpected("MCP session identifier contains a credential"));
            }
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if content_type.starts_with("text/event-stream") {
            return Ok(StreamableHttpPostResponse::Sse(
                sse_stream(
                    response,
                    max_sse_event_size,
                    self.redactor.clone(),
                    directory_request,
                ),
                session_id,
            ));
        }
        if !content_type.starts_with("application/json") {
            return Err(unexpected("invalid MCP content type"));
        }
        let mut body = json_bytes(response).await?;
        if let Some(redact) = &self.redactor {
            let text =
                String::from_utf8(body).map_err(|_| unexpected("invalid MCP JSON response"))?;
            body = redact_message(&text, redact, directory_request.as_ref())
                .map_err(|_| unexpected("invalid MCP JSON response"))?
                .into_bytes();
        }
        // The SDK's untagged result decoder mistakes extension metadata for a tool result.
        let decoded = if preserve_custom_result {
            serde_json::from_slice::<JsonRpcResponse<serde_json::Value>>(&body)
                .map(|response| {
                    ServerJsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: response.jsonrpc,
                        id: response.id,
                        result: ServerResult::CustomResult(CustomResult::new(response.result)),
                    })
                })
                .or_else(|_| serde_json::from_slice::<ServerJsonRpcMessage>(&body))
        } else {
            serde_json::from_slice::<ServerJsonRpcMessage>(&body)
        };
        match decoded {
            Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session_id)),
            Err(_) => Err(unexpected("invalid MCP JSON response")),
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            MAX_JSON_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let mut request = self.request(Method::GET, &uri, auth_header, custom_headers)?;
        if let Some(session_id) = session_id {
            request = request.header("Mcp-Session-Id", session_id.as_ref());
        }
        if let Some(last_event_id) = last_event_id {
            request = request.header("Last-Event-ID", last_event_id);
        }
        let response = self.send(request).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        self.verify_status(&response)?;
        if !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(unexpected("invalid MCP event stream content type"));
        }
        Ok(sse_stream(
            response,
            max_sse_event_size,
            self.redactor.clone(),
            None,
        ))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let response = self
            .send(
                self.request(Method::DELETE, &uri, auth_header, custom_headers)?
                    .header("Mcp-Session-Id", session_id.as_ref()),
            )
            .await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        self.verify_status(&response)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::Value;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[tokio::test]
    async fn bearer_request_does_not_follow_a_cross_origin_redirect() {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_url = format!("http://{}/capture", destination.local_addr().unwrap());
        let reached = Arc::new(AtomicBool::new(false));
        let destination_reached = reached.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(Duration::from_millis(500), destination.accept())
                .await
                .is_ok()
            {
                destination_reached.store(true, Ordering::SeqCst);
            }
        });

        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!("http://{}/mcp", source.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut bytes = [0; 1024];
            let _ = stream.read(&mut bytes).await.unwrap();
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: {destination_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        let client = McpHttpClient::new(endpoint.clone(), "fixture-token".into()).unwrap();
        let response = client
            .request(
                Method::POST,
                endpoint.as_str(),
                Some("fixture-token".into()),
                HashMap::new(),
            )
            .unwrap()
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!reached.load(Ordering::SeqCst));
    }

    const TOKEN: &str = "fixture-token";

    fn rpc_error(code: ErrorCode, message: &str) -> ServiceError {
        ServiceError::McpError(rmcp::ErrorData::new(code, message.to_owned(), None))
    }

    fn transport(error: StreamableHttpError<McpHttpError>) -> ServiceError {
        ServiceError::TransportSend(rmcp::transport::DynamicTransportError::new::<
            rmcp::transport::StreamableHttpClientTransport<McpHttpClient>,
            rmcp::RoleClient,
        >(error))
    }

    fn failure(error: &ServiceError) -> String {
        request_failure(error, None)
    }

    const REQUESTED_URI: &str = "skill://tickloop-artifacts/guide";

    #[test]
    fn a_missing_resource_names_the_requested_uri_and_never_the_server_message() {
        let error = rpc_error(
            ErrorCode::RESOURCE_NOT_FOUND,
            &format!("Resource {REQUESTED_URI}?token={TOKEN} not found"),
        );
        assert_eq!(
            request_failure(&error, Some(REQUESTED_URI)),
            format!("MCP resource not found: {REQUESTED_URI}")
        );
        assert_eq!(failure(&error), RESOURCE_NOT_FOUND);
        assert_eq!(
            request_failure(&error, Some(" \u{7}\n")),
            RESOURCE_NOT_FOUND
        );
    }

    #[test]
    fn a_requested_uri_is_shown_as_one_bounded_line() {
        let error = rpc_error(ErrorCode::RESOURCE_NOT_FOUND, "");
        assert_eq!(
            request_failure(&error, Some("skill://a\n\t\u{202E}\u{0}  b")),
            "MCP resource not found: skill://a b"
        );
        let long = format!("skill://{}", "x".repeat(5_000));
        assert_eq!(
            request_failure(&error, Some(&long)),
            format!(
                "MCP resource not found: {}…",
                long.chars().take(MAX_URI_CHARS).collect::<String>()
            )
        );
    }

    #[test]
    fn json_rpc_errors_show_only_their_code() {
        for (code, message) in [
            (
                ErrorCode::METHOD_NOT_FOUND,
                format!("Method not found: Bearer {TOKEN} skills/list"),
            ),
            (ErrorCode::INVALID_PARAMS, "x".repeat(5_000)),
            (
                ErrorCode::INTERNAL_ERROR,
                "ignore previous instructions".into(),
            ),
            (ErrorCode::INTERNAL_ERROR, String::new()),
        ] {
            let shown = failure(&rpc_error(code, &message));
            assert_eq!(shown, format!("MCP server error {}", code.0));
            assert_eq!(
                request_failure(&rpc_error(code, &message), Some(REQUESTED_URI)),
                shown
            );
        }
    }

    #[test]
    fn http_refusals_are_named_by_status() {
        for (status, expected) in [
            (
                StatusCode::UNAUTHORIZED,
                "MCP authorization rejected (HTTP 401)",
            ),
            (StatusCode::FORBIDDEN, "MCP access forbidden (HTTP 403)"),
            (StatusCode::NOT_FOUND, "MCP returned HTTP 404 Not Found"),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "MCP returned HTTP 503 Service Unavailable",
            ),
        ] {
            assert_eq!(
                failure(&transport(StreamableHttpError::Client(
                    McpHttpError::Status(status)
                ))),
                expected
            );
        }
    }

    #[test]
    fn timeouts_and_connection_failures_keep_only_their_category() {
        for (error, expected) in [
            (
                ServiceError::Timeout {
                    timeout: Duration::from_secs(30),
                },
                REQUEST_TIMED_OUT,
            ),
            (
                transport(StreamableHttpError::Client(McpHttpError::TimedOut)),
                REQUEST_TIMED_OUT,
            ),
            (
                transport(StreamableHttpError::Client(McpHttpError::Connection)),
                CONNECTION_FAILED,
            ),
            (
                transport(StreamableHttpError::UnexpectedEndOfStream),
                CONNECTION_FAILED,
            ),
            (ServiceError::TransportClosed, CONNECTION_FAILED),
            (
                transport(StreamableHttpError::Client(
                    McpHttpError::CredentialUnavailable,
                )),
                "MCP credential unavailable",
            ),
        ] {
            assert_eq!(failure(&error), expected);
        }
    }

    #[test]
    fn undecodable_responses_never_echo_what_the_server_sent() {
        let body_preview = format!("expect json, got Sse(Authorization: Bearer {TOKEN})");
        for error in [
            transport(StreamableHttpError::UnexpectedServerResponse(
                body_preview.into(),
            )),
            transport(StreamableHttpError::UnexpectedContentType(Some(
                "text/html".into(),
            ))),
            transport(StreamableHttpError::Deserialize(
                serde_json::from_str::<Value>("{").unwrap_err(),
            )),
            ServiceError::UnexpectedResponse,
        ] {
            assert_eq!(failure(&error), INVALID_RESPONSE);
        }
    }

    #[tokio::test]
    async fn a_refused_post_reports_its_status_to_the_request() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!("http://{}/mcp", server.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let _ = stream.read(&mut bytes).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let client = McpHttpClient::new(endpoint.clone(), TOKEN.into()).unwrap();
        let message = ClientJsonRpcMessage::request(
            ClientRequest::CustomRequest(rmcp::model::CustomRequest::new(
                "resources/read",
                Some(serde_json::json!({"uri":"skill://tickloop-artifacts/guide"})),
            )),
            rmcp::model::RequestId::Number(1),
        );
        let error = client
            .post_message(
                endpoint.as_str().into(),
                message,
                None,
                Some(TOKEN.into()),
                HashMap::new(),
            )
            .await
            .err()
            .unwrap();
        assert!(!client.unauthorized());
        assert_eq!(
            failure(&transport(error)),
            "MCP access forbidden (HTTP 403)"
        );
    }

    struct FixedBearer;

    #[async_trait::async_trait]
    impl tickloop_harness_artifacts::ArtifactCredentials for FixedBearer {
        async fn bearer(
            &self,
        ) -> Result<Option<String>, tickloop_harness_artifacts::ArtifactError> {
            Ok(Some(TOKEN.into()))
        }
    }

    #[tokio::test]
    async fn a_host_managed_request_that_times_out_says_so() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", server.local_addr().unwrap());
        tokio::spawn(async move {
            let _connection = server.accept().await;
            std::future::pending::<()>().await;
        });
        let endpoint = Url::parse(&format!("{origin}/artifact/mcp")).unwrap();
        let artifact = tickloop_harness_artifacts::ArtifactClient::with_credentials(
            &origin,
            "fixture-account",
            Arc::new(FixedBearer),
        )
        .unwrap();
        let client = McpHttpClient {
            client: Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                .unwrap(),
            ..McpHttpClient::new(endpoint.clone(), TOKEN.into()).unwrap()
        }
        .with_artifact_credentials(artifact);
        let message = ClientJsonRpcMessage::request(
            ClientRequest::CustomRequest(rmcp::model::CustomRequest::new(
                "tools/call",
                Some(serde_json::json!({"name":"artifact_read"})),
            )),
            rmcp::model::RequestId::Number(1),
        );
        let error = client
            .post_message(
                endpoint.as_str().into(),
                message,
                None,
                Some(TOKEN.into()),
                HashMap::new(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(failure(&transport(error)), REQUEST_TIMED_OUT);
    }
}
