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
    model::{
        ClientJsonRpcMessage, ClientRequest, CustomResult, JsonRpcResponse, ServerJsonRpcMessage,
        ServerResult,
    },
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::{Sse, SseStream};

use tickloop_agent_base::network::{HttpPolicy, build_http_client};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("Tickloop-Agent/", env!("CARGO_PKG_VERSION"), " MCP/1");

#[derive(Clone)]
pub(crate) struct McpHttpClient {
    client: Client,
    endpoint: Url,
    token: Arc<str>,
    unauthorized: Arc<AtomicBool>,
    artifact: Option<tickloop_agent_artifacts::ArtifactClient>,
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
            artifact: None,
        })
    }

    pub(crate) fn with_artifact_credentials(
        mut self,
        artifact: tickloop_agent_artifacts::ArtifactClient,
    ) -> Self {
        self.artifact = artifact.is_host_managed().then_some(artifact);
        self
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StreamableHttpError<reqwest::Error>> {
        let Some(artifact) = &self.artifact else {
            return request.send().await.map_err(StreamableHttpError::Client);
        };
        let credential = artifact
            .access_snapshot()
            .await
            .map_err(|_| unexpected("MCP credential unavailable"))?
            .ok_or_else(|| unexpected("MCP credential unavailable"))?;
        match artifact.send_authenticated(request, &credential).await {
            Ok(response) => Ok(response),
            Err(tickloop_agent_artifacts::ArtifactError::AccessDenied) => {
                self.unauthorized.store(true, Ordering::SeqCst);
                Err(unexpected("MCP authorization rejected (HTTP 401)"))
            }
            Err(_) => Err(unexpected("MCP request failed")),
        }
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
    ) -> Result<reqwest::RequestBuilder, StreamableHttpError<reqwest::Error>> {
        let url = Url::parse(uri).map_err(|_| unexpected("invalid MCP endpoint"))?;
        if url != self.endpoint || auth.as_deref() != Some(&self.token) {
            return Err(unexpected("MCP endpoint or authorization changed"));
        }
        let mut request = self
            .client
            .request(method, url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .bearer_auth(self.token.as_ref());
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

    fn verify_status(&self, status: StatusCode) -> Result<(), StreamableHttpError<reqwest::Error>> {
        if status == StatusCode::UNAUTHORIZED {
            self.unauthorized.store(true, Ordering::SeqCst);
            return Err(unexpected("MCP authorization rejected (HTTP 401)"));
        }
        if !status.is_success() {
            return Err(unexpected(format!("MCP returned HTTP {status}")));
        }
        Ok(())
    }
}

fn unexpected(message: impl Into<Cow<'static, str>>) -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::UnexpectedServerResponse(message.into())
}

async fn json_bytes(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(StreamableHttpError::Client)?
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
    SseStream::from_bytes_stream(stream).boxed()
}

impl StreamableHttpClient for McpHttpClient {
    type Error = reqwest::Error;

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
        self.verify_status(response.status())?;
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
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if content_type.starts_with("text/event-stream") {
            return Ok(StreamableHttpPostResponse::Sse(
                sse_stream(response, max_sse_event_size),
                session_id,
            ));
        }
        if !content_type.starts_with("application/json") {
            return Err(unexpected("invalid MCP content type"));
        }
        let body = json_bytes(response).await?;
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
        self.verify_status(response.status())?;
        if !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(unexpected("invalid MCP event stream content type"));
        }
        Ok(sse_stream(response, max_sse_event_size))
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
        self.verify_status(response.status())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

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
}
