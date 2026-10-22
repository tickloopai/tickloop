use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, mpsc::UnboundedSender};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message, client::IntoClientRequest, handshake::derive_accept_key, http::HeaderValue,
        protocol::Role as SocketRole,
    },
};
use tokio_util::sync::CancellationToken;

use super::{
    ModelDelta, ModelError, ModelOutput, ModelRequestContext, ModelRequestStage, normalize_codex_id,
};
use crate::{
    MAX_RESPONSE_STREAM_BYTES, MAX_SSE_EVENT_BYTES, provider_failure::ProviderHttpFailureExt,
    responses::ResponsesStream,
};
use tickloop_agent_protocol::{ChatId, Content, ModelConfig, Protocol, ProviderHttpFailure, Role};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const CACHE_IDLE: Duration = Duration::from_secs(5 * 60);
const CACHE_AGE: Duration = Duration::from_secs(55 * 60);

type Socket = WebSocketStream<reqwest::Upgraded>;

struct Continuation {
    body: Value,
    response_id: String,
    response_items: Vec<Value>,
}

struct CachedConnection {
    socket: Socket,
    token_fingerprint: [u8; 32],
    created: Instant,
    last_used: Instant,
    continuation: Option<Continuation>,
}

#[derive(Default)]
pub(super) struct WebSocketCache {
    entries: Mutex<HashMap<(ChatId, String), CachedConnection>>,
}

impl WebSocketCache {
    pub(super) async fn close_chat(&self, chat_id: ChatId) {
        self.entries
            .lock()
            .await
            .retain(|(chat, _), _| *chat != chat_id);
    }

    async fn take(
        &self,
        chat_id: ChatId,
        account_id: &str,
        token_fingerprint: &[u8; 32],
    ) -> Option<CachedConnection> {
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        entries.retain(|(chat, account), entry| {
            let same_chat = *chat == chat_id;
            let compatible = !same_chat
                || account == account_id && &entry.token_fingerprint == token_fingerprint;
            compatible
                && now.duration_since(entry.created) < CACHE_AGE
                && now.duration_since(entry.last_used) < CACHE_IDLE
        });
        entries.remove(&(chat_id, account_id.to_owned()))
    }

    async fn store(&self, chat_id: ChatId, account_id: &str, entry: CachedConnection) {
        self.entries
            .lock()
            .await
            .insert((chat_id, account_id.to_owned()), entry);
    }
}

pub(super) struct WebSocketFailure {
    pub(super) error: ModelError,
    pub(super) may_fallback: bool,
}

pub(super) struct WebSocketRequest<'a> {
    pub(super) config: &'a ModelConfig,
    pub(super) client: &'a reqwest::Client,
    pub(super) chat_id: Option<ChatId>,
    pub(super) endpoint: &'a Url,
    pub(super) body: &'a Value,
    pub(super) access_token: &'a str,
    pub(super) account_id: &'a str,
}

fn failure(error: ModelError, started: bool) -> WebSocketFailure {
    let may_fallback = !started
        && matches!(
            error.kind(),
            crate::ModelFailureKind::Connection
                | crate::ModelFailureKind::StreamInterrupted
                | crate::ModelFailureKind::Timeout
        );
    let error = if started {
        match error {
            ModelError::Connection
            | ModelError::ConnectionAt { .. }
            | ModelError::StreamInterrupted
            | ModelError::TimedOut
            | ModelError::TimedOutAt { .. } => ModelError::StreamInterruptedAfterOutput,
            ModelError::HttpResponse {
                status,
                description,
                retry_after,
                ..
            } => ModelError::HttpResponse {
                status,
                description,
                retry_after,
                should_retry: Some(false),
            },
            other => other,
        }
    } else {
        error
    };
    WebSocketFailure {
        error,
        may_fallback,
    }
}

pub(super) async fn complete(
    cache: &WebSocketCache,
    request: WebSocketRequest<'_>,
    events: UnboundedSender<ModelDelta>,
    cancel: CancellationToken,
    model_context: Option<&ModelRequestContext>,
) -> Result<ModelOutput, WebSocketFailure> {
    let WebSocketRequest {
        config,
        client,
        chat_id,
        endpoint,
        body: full_body,
        access_token,
        account_id,
    } = request;
    let token_fingerprint: [u8; 32] = Sha256::digest(access_token.as_bytes()).into();
    let mut cached = match chat_id {
        Some(chat) => cache.take(chat, account_id, &token_fingerprint).await,
        None => None,
    };
    if cached.is_none() {
        let mut url = endpoint.clone();
        url.set_scheme(if endpoint.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| {
            failure(
                ModelError::Configuration("invalid Codex WebSocket URL".into()),
                false,
            )
        })?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| failure(ModelError::Connection, false))?;
        let headers = request.headers_mut();
        let mut bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| failure(ModelError::CodexAuthentication, false))?;
        bearer.set_sensitive(true);
        headers.insert("authorization", bearer);
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(account_id)
                .map_err(|_| failure(ModelError::CodexAuthentication, false))?,
        );
        headers.insert(
            "originator",
            HeaderValue::from_static(super::CODEX_CLIENT_ORIGINATOR),
        );
        headers.insert(
            "OpenAI-Beta",
            HeaderValue::from_static("responses_websockets=2026-02-06"),
        );
        headers.insert(
            "user-agent",
            HeaderValue::from_static(concat!("tickloop-agent/", env!("CARGO_PKG_VERSION"))),
        );
        if let Some(chat) = chat_id {
            let session = HeaderValue::from_str(&chat.to_string())
                .map_err(|_| failure(ModelError::Connection, false))?;
            headers.insert("session-id", session.clone());
            headers.insert("x-client-request-id", session);
        }
        let expected_accept = derive_accept_key(request.headers()["sec-websocket-key"].as_bytes());
        let outgoing = client
            .get(endpoint.clone())
            .version(http::Version::HTTP_11)
            .headers(request.headers().clone());
        let mut response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(failure(ModelError::Cancelled, false)),
            result = tokio::time::timeout(CONNECT_TIMEOUT, outgoing.send()) => result
                .map_err(|_| failure(ModelError::TimedOutAt { stage: ModelRequestStage::Connect }, false))?
                .map_err(|error| failure(crate::request_transport_error(error), false))?,
        };
        let status = response.status().as_u16();
        if status != 101 {
            let (retry_after, server_retry) =
                crate::provider_failure::retry_headers(response.headers());
            let error_body = crate::provider_failure::read_error_body(&mut response, &cancel)
                .await
                .map_err(|error| failure(error, false))?;
            let bytes = &error_body.bytes;
            let mut detail = ProviderHttpFailure::from_response(
                config,
                status,
                bytes,
                retry_after,
                server_retry,
                &[access_token, account_id],
            );
            let may_fallback = error_body.complete
                && bytes.is_empty()
                && matches!(status, 400 | 404 | 405 | 426 | 501);
            detail.require_complete_body(error_body.complete, &[access_token, account_id]);
            if may_fallback {
                detail.retryable = false;
            }
            return Err(WebSocketFailure {
                error: ModelError::ProviderHttp(Box::new(detail)),
                may_fallback,
            });
        }
        let headers = response.headers();
        let has_upgrade = headers
            .get("upgrade")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        let has_connection_upgrade = headers
            .get("connection")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            });
        if !has_upgrade
            || !has_connection_upgrade
            || headers
                .get("sec-websocket-accept")
                .is_none_or(|value| value.as_bytes() != expected_accept.as_bytes())
            || headers.contains_key("sec-websocket-protocol")
            || headers.contains_key("sec-websocket-extensions")
        {
            return Err(failure(
                ModelError::ConnectionAt {
                    stage: ModelRequestStage::Connect,
                },
                false,
            ));
        }
        let upgraded = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(failure(ModelError::Cancelled, false)),
            result = tokio::time::timeout(CONNECT_TIMEOUT, response.upgrade()) => result
                .map_err(|_| failure(ModelError::TimedOutAt { stage: ModelRequestStage::Connect }, false))?
                .map_err(|error| failure(crate::request_transport_error(error), false))?,
        };
        let socket = WebSocketStream::from_raw_socket(upgraded, SocketRole::Client, None).await;
        cached = Some(CachedConnection {
            socket,
            token_fingerprint,
            created: Instant::now(),
            last_used: Instant::now(),
            continuation: None,
        });
    }
    let mut entry = cached.expect("connection established");
    let delta_body = entry
        .continuation
        .as_ref()
        .and_then(|previous| continuation_body(full_body, previous));
    let mut request_body = delta_body.unwrap_or_else(|| full_body.clone());
    let mut retried_missing_continuation = false;
    loop {
        let mut frame = request_body.clone();
        frame["type"] = json!("response.create");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(failure(ModelError::Cancelled, false)),
            result = tokio::time::timeout(IDLE_TIMEOUT, entry.socket.send(Message::Text(frame.to_string().into()))) => {
                result.map_err(|_| failure(ModelError::TimedOutAt { stage: ModelRequestStage::RequestSend }, false))?
                    .map_err(|_| failure(ModelError::ConnectionAt { stage: ModelRequestStage::RequestSend }, false))?;
            }
        }
        let mut stream = ResponsesStream::for_codex()
            .with_codex_origin(&super::codex_origin(&config.model, account_id));
        let mut structured_response =
            crate::structured_response::StructuredResponsePolicy::for_codex_body(&request_body);
        let mut started = false;
        let mut received_bytes = 0usize;
        loop {
            let incoming = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(failure(ModelError::Cancelled, started)),
                result = tokio::time::timeout(IDLE_TIMEOUT, entry.socket.next()) => result,
            };
            let message = incoming
                .map_err(|_| {
                    failure(
                        ModelError::TimedOutAt {
                            stage: ModelRequestStage::ResponseStream,
                        },
                        started,
                    )
                })?
                .ok_or_else(|| failure(ModelError::StreamInterrupted, started))?
                .map_err(|_| failure(ModelError::StreamInterrupted, started))?;
            let bytes = match message {
                Message::Text(text) => text.as_bytes().to_vec(),
                Message::Binary(bytes) => bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => return Err(failure(ModelError::StreamInterrupted, started)),
            };
            received_bytes = received_bytes.saturating_add(bytes.len());
            if bytes.len() > MAX_SSE_EVENT_BYTES || received_bytes > MAX_RESPONSE_STREAM_BYTES {
                return Err(failure(
                    ModelError::InvalidResponse(
                        "Codex WebSocket response exceeded its size limit".into(),
                    ),
                    started,
                ));
            }
            let mut data: Value = serde_json::from_slice(&bytes).map_err(|_| {
                failure(
                    ModelError::InvalidResponse("invalid Codex WebSocket JSON".into()),
                    started,
                )
            })?;
            if let Some(policy) = &mut structured_response {
                policy
                    .check_event(Protocol::Responses, &data)
                    .map_err(|error| failure(error, true))?;
            }
            if is_missing_continuation(&data) {
                if !started
                    && request_body.get("previous_response_id").is_some()
                    && !retried_missing_continuation
                    && model_context.is_none_or(ModelRequestContext::try_begin_attempt)
                {
                    request_body = full_body.clone();
                    retried_missing_continuation = true;
                    entry.continuation = None;
                    break;
                }
                return Err(failure(
                    ModelError::ResponseRejected(
                        crate::ModelResponseRejection::UnsupportedContinuation,
                    ),
                    started,
                ));
            }
            started |= event_starts_output(&data);
            if data["type"] == "error" {
                return Err(failure(
                    error_from_event(&data).at_stage(ModelRequestStage::ResponseStream),
                    started,
                ));
            }
            if data["type"] == "response.failed"
                || data["type"] == "response.done" && data["response"]["status"] == "failed"
            {
                return Err(failure(
                    error_from_event(&data).at_stage(ModelRequestStage::ResponseStream),
                    started,
                ));
            }
            if data["type"] == "response.done" {
                let status = data["response"]["status"].as_str().ok_or_else(|| {
                    failure(
                        ModelError::InvalidResponse("Codex response ended without a status".into()),
                        true,
                    )
                })?;
                if !matches!(status, "completed" | "failed" | "incomplete" | "cancelled") {
                    return Err(failure(
                        ModelError::InvalidResponse("unsupported Codex response status".into()),
                        true,
                    ));
                }
                data["type"] = json!(format!("response.{status}"));
            }
            if stream
                .accept(data, &events)
                .map_err(|error| failure(error, true))?
            {
                let response_id = stream.response_id().unwrap_or_default().to_owned();
                let output = stream.finish().map_err(|error| failure(error, true))?;
                if let Some(chat) = chat_id {
                    let response_items = response_items(&output);
                    entry.continuation = Some(Continuation {
                        body: full_body.clone(),
                        response_id,
                        response_items,
                    });
                    entry.last_used = Instant::now();
                    cache.store(chat, account_id, entry).await;
                }
                return Ok(output);
            }
        }
    }
}

fn continuation_body(body: &Value, previous: &Continuation) -> Option<Value> {
    let mut comparable = body.clone();
    let mut old = previous.body.clone();
    comparable.as_object_mut()?.remove("input");
    old.as_object_mut()?.remove("input");
    if comparable != old {
        return None;
    }
    let current = body["input"].as_array()?;
    let mut baseline = previous.body["input"].as_array()?.clone();
    baseline.extend(previous.response_items.clone());
    if current.len() < baseline.len() || current[..baseline.len()] != baseline {
        return None;
    }
    let mut delta = body.clone();
    delta["previous_response_id"] = json!(previous.response_id);
    delta["input"] = json!(current[baseline.len()..].to_vec());
    Some(delta)
}

fn response_items(output: &ModelOutput) -> Vec<Value> {
    let mut items = Vec::new();
    for content in &output.content {
        match content {
            Content::Text { text } => {
                items.push(json!({"type":"message","role":Role::Assistant,"content":text}))
            }
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value,
            } if value["type"] == "reasoning" => {
                let mut item = value.clone();
                item.as_object_mut()
                    .unwrap()
                    .remove(super::CODEX_ORIGIN_FIELD);
                items.push(item);
            }
            Content::ToolUse { id, name, input } => {
                let (call_id, item_id) = id
                    .split_once('|')
                    .map_or((id.as_str(), None), |(call, item)| (call, Some(item)));
                let mut item = json!({"type":"function_call","call_id":normalize_codex_id(call_id, "call"),"name":name,"arguments":input.to_string()});
                if let Some(item_id) = item_id {
                    item["id"] = json!(normalize_codex_id(item_id, "fc"));
                }
                items.push(item);
            }
            _ => {}
        }
    }
    items
}

fn is_missing_continuation(data: &Value) -> bool {
    let code = data["error"]["code"]
        .as_str()
        .or_else(|| data["response"]["error"]["code"].as_str());
    matches!(
        code,
        Some("previous_response_id_not_found" | "invalid_previous_response_id")
    )
}

fn event_starts_output(data: &Value) -> bool {
    data["type"].as_str().is_some_and(|kind| {
        kind.starts_with("response.output_")
            || kind.starts_with("response.reasoning_")
            || kind.starts_with("response.function_call_")
    })
}

fn error_from_event(data: &Value) -> ModelError {
    let error = data.get("error").or_else(|| data["response"].get("error"));
    let inferred_status = error
        .and_then(|error| error["code"].as_str().or_else(|| error["type"].as_str()))
        .and_then(super::errors::inferred_status_for_code)
        .unwrap_or(500);
    let status = error
        .and_then(|error| error["status"].as_u64())
        .or_else(|| data["status"].as_u64())
        .unwrap_or(u64::from(inferred_status))
        .min(u16::MAX as u64) as u16;
    let body = error.map(|error| json!({"error":error}));
    super::errors::http_error(status, body.as_ref(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_type_only_quota_has_terminal_429_status() {
        for code in [
            "usage_limit_reached",
            "GoUsageLimitError",
            "FreeUsageLimitError",
        ] {
            let error = error_from_event(&json!({
                "type":"error",
                "error":{"type":code,"resets_at":42}
            }));
            assert_eq!(error.kind(), crate::ModelFailureKind::QuotaExhausted);
            assert_eq!(error.status(), Some(429));
            assert!(!error.retry_hint().0);
        }
    }
}
