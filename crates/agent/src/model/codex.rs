use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderValue, RETRY_AFTER},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{
    CODEX_RESPONSES_URL, Content, ModelAuthSource, ModelClient, ModelConfig, ModelDelta,
    ModelError, ModelIdentity, ModelOutput, ModelProvider, ModelRequest, ModelRequestContext,
    ModelRequestStage, ModelResponseRejection, Protocol, ReasoningEffort, Role,
    consume_sse_response, image_outside_user_message, is_context_limit_error,
    request_transport_error, responses, validate_reasoning_effort,
};
use crate::auth::{AuthError, CodexAuth};
use crate::network::{HttpPolicy, NetworkDiagnostic, NetworkRoute, build_http_client};

const CODEX_CLIENT_ORIGINATOR: &str = "tickloop";

#[cfg(test)]
mod error_tests;
mod errors;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod tool_locality_tests;
mod transport;
const CODEX_ORIGIN_FIELD: &str = "_tickloop_codex_origin";

pub(super) fn validate_config(config: &ModelConfig) -> Result<(), ModelError> {
    if config.provider != ModelProvider::OpenAiCodex
        || config.auth_source != ModelAuthSource::CodexSubscription
        || config.protocol != Protocol::Responses
        || config.base_url != CODEX_RESPONSES_URL
        || !config.api_key_env.is_empty()
        || config.name.trim().is_empty()
        || config.model.trim().is_empty()
        || config.max_output_tokens == 0
    {
        return Err(ModelError::Configuration(
            "Codex subscription requires the fixed official endpoint, Responses protocol and subscription authentication".into(),
        ));
    }
    Ok(())
}

pub(super) struct CodexModelClient {
    config: ModelConfig,
    auth: CodexAuth,
    client: reqwest::Client,
    route: NetworkRoute,
    endpoint: Url,
    websocket_cache: transport::WebSocketCache,
    prefer_websocket: bool,
}

impl CodexModelClient {
    pub(super) fn new(config: ModelConfig, state_dir: &Path) -> Result<Self, ModelError> {
        validate_config(&config)?;
        let endpoint = Url::parse(CODEX_RESPONSES_URL).expect("fixed official endpoint");
        let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
        policy.connect_timeout = Some(Duration::from_secs(15));
        policy.read_timeout = Some(Duration::from_secs(120));
        let (client, route) =
            build_http_client(&endpoint, policy).map_err(|_| ModelError::Connection)?;
        let prefer_websocket = route.direct_websocket_eligible;
        Ok(Self {
            config,
            auth: CodexAuth::new(state_dir),
            client,
            route,
            endpoint,
            websocket_cache: transport::WebSocketCache::default(),
            prefer_websocket,
        })
    }

    #[cfg(test)]
    pub(super) fn with_loopback_test_auth(
        mut self,
        state_dir: &Path,
        origin: &str,
    ) -> Result<Self, ModelError> {
        self.auth = CodexAuth::new_for_loopback_test(state_dir, origin).map_err(|_| {
            ModelError::Configuration(
                "Codex test OAuth origin must be numeric loopback HTTP".into(),
            )
        })?;
        Ok(self)
    }

    #[cfg(test)]
    pub(super) fn with_loopback_test_endpoint(
        mut self,
        endpoint: &str,
    ) -> Result<Self, ModelError> {
        let url = Url::parse(endpoint).map_err(|_| {
            ModelError::Configuration("Codex test endpoint must be numeric loopback HTTP".into())
        })?;
        let is_numeric_loopback = url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|address| address.is_loopback());
        if url.scheme() != "http"
            || !is_numeric_loopback
            || url.port().is_none()
            || url.path() != "/codex/responses"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ModelError::Configuration(
                "Codex test endpoint must be numeric loopback HTTP".into(),
            ));
        }
        let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
        policy.connect_timeout = Some(Duration::from_secs(15));
        policy.read_timeout = Some(Duration::from_secs(120));
        (self.client, self.route) =
            build_http_client(&url, policy).map_err(|_| ModelError::Connection)?;
        self.endpoint = url;
        self.prefer_websocket = false;
        Ok(self)
    }

    #[cfg(test)]
    pub(super) fn with_loopback_test_websocket(mut self) -> Self {
        self.prefer_websocket = true;
        self
    }
}

impl CodexModelClient {
    async fn complete_inner(
        &self,
        chat_id: Option<crate::runtime::ChatId>,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        model_context: Option<&ModelRequestContext>,
    ) -> Result<ModelOutput, ModelError> {
        let structured_tool = request
            .structured_output
            .as_ref()
            .map(|format| format.name.clone());
        let credentials = self
            .auth
            .resolve_credentials_with_refresh_gate(&cancel, || {
                model_context.is_none_or(ModelRequestContext::try_begin_attempt)
            })
            .await
            .map_err(|error| errors::auth_error(&error))?;
        let mut source = self.config.private_history_source();
        if let Some(source) = &mut source {
            source.credential_identity = Some(super::credential_fingerprint(
                credentials.account_id.as_bytes(),
            ));
        }
        let request = super::history::project_request(request, &self.config, source.as_ref());
        let origin = codex_origin(&self.config.model, &credentials.account_id);
        let mut body = encode_request(&self.config, request, &origin)?;
        if let Some(chat) = chat_id {
            body["prompt_cache_key"] = json!(chat.to_string());
        }
        let mut output = if self.prefer_websocket {
            match transport::complete(
                &self.websocket_cache,
                transport::WebSocketRequest {
                    config: &self.config,
                    client: &self.client,
                    chat_id,
                    endpoint: &self.endpoint,
                    body: &body,
                    access_token: &credentials.access_token,
                    account_id: &credentials.account_id,
                },
                events.clone(),
                cancel.clone(),
                model_context,
            )
            .await
            {
                Ok(output) => output,
                Err(failure)
                    if failure.may_fallback
                        && !cancel.is_cancelled()
                        && model_context.is_none_or(ModelRequestContext::try_begin_attempt) =>
                {
                    self.complete_sse(
                        chat_id,
                        &body,
                        &credentials.access_token,
                        &credentials.account_id,
                        events,
                        cancel,
                    )
                    .await?
                }
                Err(failure) => return Err(failure.error),
            }
        } else {
            self.complete_sse(
                chat_id,
                &body,
                &credentials.access_token,
                &credentials.account_id,
                events,
                cancel,
            )
            .await?
        };
        output.private_source = source;
        tag_private_output(&mut output, &origin);
        match structured_tool {
            Some(name) => normalize_structured_output(output, &name)
                .map_err(super::structured_response::verified_format_error),
            None => Ok(output),
        }
    }

    async fn complete_sse(
        &self,
        chat_id: Option<crate::runtime::ChatId>,
        body: &Value,
        access_token: &str,
        account_id: &str,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let mut bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| ModelError::CodexAuthentication)?;
        bearer.set_sensitive(true);
        let mut outgoing = self
            .client
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, bearer)
            .header("chatgpt-account-id", account_id)
            .header("originator", CODEX_CLIENT_ORIGINATOR)
            .header("OpenAI-Beta", "responses=experimental")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(
                reqwest::header::USER_AGENT,
                concat!("tickloop-agent/", env!("CARGO_PKG_VERSION")),
            )
            .json(&body);
        if let Some(chat) = chat_id {
            outgoing = outgoing
                .header("session-id", chat.to_string())
                .header("x-client-request-id", chat.to_string());
        }
        let started = Instant::now();
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            result = outgoing.send() => result.map_err(request_transport_error),
        };
        NetworkDiagnostic::new(
            "model_response_headers",
            &self.endpoint,
            self.route,
            sent.as_ref()
                .ok()
                .map(|response| response.status().as_u16()),
            started.elapsed(),
        )
        .emit();
        let mut response = sent?;
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(super::provider_failure::parse_retry_after);
            let server_retry = response
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| match value {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                });
            let error_body =
                super::provider_failure::read_error_body(&mut response, &cancel).await?;
            let error_bytes = &error_body.bytes;
            let body = serde_json::from_slice::<Value>(error_bytes).ok();
            if error_body.complete && body.as_ref().is_some_and(is_context_limit_error) {
                return Err(
                    ModelError::ResponseRejected(ModelResponseRejection::ContextLimit)
                        .at_stage(ModelRequestStage::ResponseHeaders),
                );
            }
            let mut failure = super::ProviderHttpFailure::from_response(
                &self.config,
                status,
                error_bytes,
                retry_after,
                server_retry,
                &[access_token, account_id],
            );
            let classified = errors::http_error(status, body.as_ref(), retry_after, server_retry);
            if let Some(body) = body.as_ref()
                && body["error"]["resets_at"].is_number()
                && let Some(detail) = classified.safe_quota_detail()
            {
                failure.hint.push_str(detail);
            }
            failure.require_complete_body(error_body.complete, &[access_token, account_id]);
            return Err(ModelError::ProviderHttp(Box::new(failure)));
        }
        consume_sse_response(
            response,
            Protocol::Responses,
            events,
            cancel,
            super::structured_response::StructuredResponsePolicy::for_codex_body(body),
            super::cards::Adapter::CodexResponses,
            false,
        )
        .await
    }
}

#[async_trait]
impl ModelClient for CodexModelClient {
    fn connection_id(&self) -> Option<String> {
        Some(self.config.provider_id.clone())
    }

    fn identity(&self) -> ModelIdentity {
        self.config.identity()
    }

    async fn close_chat(&self, chat_id: crate::runtime::ChatId) {
        self.websocket_cache.close_chat(chat_id).await;
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(None, request, events, cancel, None)
            .await
    }

    async fn complete_for_chat(
        &self,
        chat_id: crate::runtime::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(Some(chat_id), request, events, cancel, None)
            .await
    }

    async fn complete_for_chat_with_context(
        &self,
        chat_id: crate::runtime::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        context: ModelRequestContext,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(Some(chat_id), request, events, cancel, Some(&context))
            .await
    }
}

fn encode_request(
    config: &ModelConfig,
    request: ModelRequest,
    origin: &str,
) -> Result<Value, ModelError> {
    validate_reasoning_effort(config, request.reasoning_effort)?;
    let mut request = super::without_unsupported_images(config, request);
    super::image_limits::limit_images_per_request(&mut request.messages);
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    let mut seen_non_system = false;
    let mut tool_calls = HashMap::new();
    let mut pending_tool_results = HashSet::new();
    let mut wire_call_ids = HashSet::new();
    for message in request.messages {
        if message.role == Role::System {
            if seen_non_system {
                return Err(ModelError::Configuration(
                    "system instructions must precede conversation history".into(),
                ));
            }
            for content in message.content {
                let Content::Text { text } = content else {
                    return Err(ModelError::Configuration(
                        "system instructions must be text".into(),
                    ));
                };
                instructions.push(text);
            }
            continue;
        }
        seen_non_system = true;
        let has_image = message
            .content
            .iter()
            .any(|content| matches!(content, Content::Image { .. }));
        if has_image && message.role != Role::User {
            return Err(image_outside_user_message());
        }
        // A user turn with images keeps its text and images together in one content array.
        let mut user_parts = Vec::new();
        let mut assistant_phase = None;
        for content in message.content {
            let joins_user_parts = matches!(content, Content::Image { .. })
                || has_image && matches!(content, Content::Text { .. });
            if !joins_user_parts {
                responses::push_user_parts(&mut input, &mut user_parts);
            }
            match content {
                Content::Text { text } if has_image => {
                    user_parts.push(json!({"type":"input_text","text":text}))
                }
                Content::Image { source, .. } => {
                    user_parts.push(json!({"type":"input_image","image_url":source.data_url()}))
                }
                Content::Text { text } if message.role == Role::Assistant => {
                    input.push(responses::assistant_text_item(text, assistant_phase.take()));
                }
                Content::Text { text } => {
                    input.push(json!({"type":"message","role":message.role,"content":text}))
                }
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value,
                } if value["type"] == responses::ASSISTANT_PHASE_TYPE
                    && message.role == Role::Assistant
                    && value[CODEX_ORIGIN_FIELD].as_str() == Some(origin) =>
                {
                    assistant_phase = Some(super::required_string(&value, "phase")?.to_owned());
                }
                Content::ToolUse {
                    id,
                    name,
                    input: arguments,
                } => {
                    let mut id_parts = id.split('|');
                    let raw_call_id = id_parts.next().unwrap_or_default();
                    let raw_item_id = id_parts.next();
                    let item_origin = id_parts.next();
                    if id_parts.next().is_some() {
                        return Err(ModelError::Configuration(
                            "Codex history contains an invalid tool ID".into(),
                        ));
                    }
                    if raw_call_id.is_empty() || name.is_empty() || !arguments.is_object() {
                        return Err(ModelError::Configuration(
                            "Codex history contains an invalid tool call".into(),
                        ));
                    }
                    let call_id = normalize_codex_id(raw_call_id, "call");
                    if !wire_call_ids.insert(call_id.clone())
                        || tool_calls.insert(id.clone(), call_id.clone()).is_some()
                    {
                        return Err(ModelError::Configuration(
                            "Codex history contains duplicate tool IDs".into(),
                        ));
                    }
                    pending_tool_results.insert(id.clone());
                    let mut item = json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments.to_string()});
                    if let Some(raw_item_id) = raw_item_id.filter(|_| item_origin == Some(origin)) {
                        item["id"] = json!(normalize_codex_id(raw_item_id, "fc"));
                    }
                    input.push(item);
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let call_id = tool_calls.get(&tool_use_id).ok_or_else(|| {
                        ModelError::Configuration(
                            "Codex history contains an unpaired tool result".into(),
                        )
                    })?;
                    if !pending_tool_results.remove(&tool_use_id) {
                        return Err(ModelError::Configuration(
                            "Codex history contains a duplicate tool result".into(),
                        ));
                    }
                    let output = responses::function_call_output(content);
                    input.push(
                        json!({"type":"function_call_output","call_id":call_id,"output":output}),
                    );
                }
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value,
                } if value["type"] == "reasoning"
                    && value[CODEX_ORIGIN_FIELD].as_str() == Some(origin) =>
                {
                    let mut value = value;
                    value.as_object_mut().unwrap().remove(CODEX_ORIGIN_FIELD);
                    input.push(value);
                }
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value,
                } if value["type"] == "reasoning"
                    || value["type"] == responses::ASSISTANT_PHASE_TYPE => {}
                _ => {
                    return Err(ModelError::Configuration(
                        "Codex subscription history contains unsupported content".into(),
                    ));
                }
            }
        }
        responses::push_user_parts(&mut input, &mut user_parts);
    }
    if !pending_tool_results.is_empty() {
        return Err(ModelError::Configuration(
            "Codex history contains an unfinished tool call".into(),
        ));
    }
    let mut body = json!({
        "model": config.model,
        "instructions": if instructions.is_empty() { "You are a helpful assistant.".to_owned() } else { instructions.join("\n\n") },
        "input": input,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "low" },
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });
    if let Some(format) = request.structured_output {
        if !request.tools.is_empty()
            || format.name.trim().is_empty()
            || format.schema["type"] != "object"
        {
            return Err(ModelError::Configuration(
                "Codex structured output requires one named object schema without business tools"
                    .into(),
            ));
        }
        body["tools"] = json!([{
            "type": "function",
            "name": format.name,
            "description": "Return the structured result.",
            "parameters": format.schema,
            "strict": true,
        }]);
        body["tool_choice"] = json!("required");
        body["parallel_tool_calls"] = json!(false);
    } else if !request.tools.is_empty() {
        body["tools"] = json!(
            request
                .tools
                .into_iter()
                .map(|tool| json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": null,
                }))
                .collect::<Vec<_>>()
        );
    }
    if let Some(effort) = request.reasoning_effort {
        body["reasoning"] = json!({
            "effort": match effort { ReasoningEffort::Off => "none", _ => effort.label() },
            "summary": "auto",
        });
    }
    Ok(body)
}

fn normalize_codex_id(raw: &str, prefix: &str) -> String {
    let is_valid = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && (prefix != "fc" || raw.starts_with("fc_"));
    if is_valid {
        return raw.to_owned();
    }
    let hash = Sha256::digest(raw.as_bytes());
    let suffix = hash[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}_{suffix}")
}

fn codex_origin(model: &str, account_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"tickloop-codex-origin-v1\0");
    digest.update(model.as_bytes());
    digest.update(b"\0");
    digest.update(account_id.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn tag_private_output(output: &mut ModelOutput, origin: &str) {
    for content in &mut output.content {
        match content {
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value,
            } if value["type"] == "reasoning"
                || value["type"] == responses::ASSISTANT_PHASE_TYPE =>
            {
                if let Some(item) = value.as_object_mut() {
                    item.insert(CODEX_ORIGIN_FIELD.into(), json!(origin));
                }
            }
            Content::ToolUse { id, .. } if id.contains('|') => id.push_str(&format!("|{origin}")),
            _ => {}
        }
    }
}

fn normalize_structured_output(
    mut output: ModelOutput,
    tool_name: &str,
) -> Result<ModelOutput, ModelError> {
    let tool_calls = output
        .content
        .iter()
        .filter_map(|content| match content {
            Content::ToolUse { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if tool_calls.iter().any(|name| *name != tool_name)
        || tool_calls.len() > 1
        || output
            .content
            .iter()
            .any(|content| matches!(content, Content::ToolResult { .. }))
    {
        return Err(ModelError::ResponseRejected(
            ModelResponseRejection::UnexpectedToolOperation,
        ));
    }
    if output.stop_reason != "tool_use" {
        return Err(ModelError::InvalidResponse(
            "structured output did not finish with the required tool".into(),
        ));
    }
    let mut result = None;
    for content in &output.content {
        match content {
            Content::ToolUse { name, input, .. } if name == tool_name && result.is_none() => {
                result = Some(input);
            }
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value,
            } if value["type"] == "reasoning" => {}
            Content::Thinking { .. } => {}
            Content::ToolUse { .. } | Content::ToolResult { .. } => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::UnexpectedToolOperation,
                ));
            }
            _ => {
                return Err(ModelError::InvalidResponse(
                    "structured output contained extra content or tool operations".into(),
                ));
            }
        }
    }
    let result = result.ok_or_else(|| {
        ModelError::InvalidResponse("structured output tool was not called exactly once".into())
    })?;
    output.content = vec![Content::Text {
        text: result.to_string(),
    }];
    output.stop_reason = "end_turn".into();
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
    };
    use tokio_tungstenite::{
        accept_async, accept_hdr_async,
        tungstenite::{
            Message as WsMessage,
            handshake::server::{Request as WsRequest, Response as WsResponse},
        },
    };

    use super::*;
    use crate::{
        model::cards::ModelCatalog,
        permissions::{Decision, ExecutionMode},
        runtime::{Agent, EventKind, RequestKind},
    };

    #[test]
    fn auto_review_request_requires_one_strict_schema_tool() {
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let schema = json!({"type":"object","properties":{"shouldBlock":{"type":"boolean"}},"required":["shouldBlock"],"additionalProperties":false});
        let body = encode_request(
            &config,
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![crate::model::Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Review".into(),
                    }],
                }],
                tools: Vec::new(),
                structured_output: Some(crate::model::StructuredOutput {
                    name: "classify_result".into(),
                    schema: schema.clone(),
                }),
                temperature: Some(0.0),
                reasoning_effort: None,
            },
            "test-origin",
        )
        .unwrap();
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            body["tools"],
            json!([{"type":"function","name":"classify_result","description":"Return the structured result.","parameters":schema,"strict":true}])
        );
        assert!(body["text"]["format"].is_null());
    }

    #[test]
    fn normalized_tool_call_ids_stay_paired_with_results() {
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let wire_id = "call with spaces|fc_known|test-origin".to_owned();
        let body = encode_request(
            &config,
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    crate::model::Message {
                        role: Role::Assistant,
                        content: vec![Content::ToolUse {
                            id: wire_id.clone(),
                            name: "Read".into(),
                            input: json!({"file_path":"notes.txt"}),
                        }],
                    },
                    crate::model::Message {
                        role: Role::User,
                        content: vec![Content::ToolResult {
                            tool_use_id: wire_id,
                            content: "Friday".into(),
                            is_error: false,
                        }],
                    },
                ],
                tools: Vec::new(),
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            "test-origin",
        )
        .unwrap();
        let call_id = body["input"][0]["call_id"].as_str().unwrap();
        assert!(call_id.starts_with("call_"));
        assert!(body["input"][0]["id"].as_str().unwrap().starts_with("fc_"));
        assert_eq!(body["input"][1]["call_id"], call_id);
        assert_eq!(body["input"][1]["output"], "Friday");
    }

    #[test]
    fn assistant_phase_replays_only_for_the_same_codex_origin() {
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| profile.provider == ModelProvider::OpenAiCodex)
            .unwrap();
        let mut output = ModelOutput {
            private_source: None,
            content: vec![
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value: json!({"type":responses::ASSISTANT_PHASE_TYPE,"phase":"commentary"}),
                },
                Content::Text {
                    text: "Checking.".into(),
                },
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value: json!({"type":responses::ASSISTANT_PHASE_TYPE,"phase":"final_answer"}),
                },
                Content::Text {
                    text: "Done.".into(),
                },
                Content::Text {
                    text: "Legacy response.".into(),
                },
            ],
            usage: super::super::Usage::default(),
            stop_reason: "end_turn".into(),
        };
        tag_private_output(&mut output, "account-a");
        let mut request = simple_request();
        request.messages = vec![crate::model::Message {
            role: Role::Assistant,
            content: output.content,
        }];
        let same = encode_request(&config, request.clone(), "account-a").unwrap();
        assert_eq!(same["input"][0]["phase"], "commentary");
        assert_eq!(same["input"][1]["phase"], "final_answer");
        assert!(same["input"][2].get("phase").is_none());
        let other = encode_request(&config, request.clone(), "account-b").unwrap();
        assert!(
            other["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item.get("phase").is_none())
        );
        assert_eq!(other["input"][0]["content"], "Checking.");
        assert_eq!(encode_request(&config, request, "account-a").unwrap(), same);
    }

    #[test]
    fn codex_origin_filters_private_history_without_rewriting_transcript() {
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let current = codex_origin(&config.model, "account-one");
        let other_account = codex_origin(&config.model, "account-two");
        let other_model = codex_origin("gpt-5.6-sol", "account-one");
        assert_ne!(current, other_account);
        assert_ne!(current, other_model);
        assert!(!current.contains("account-one"));
        let reasoning = |tag: Option<&str>, id: &str| {
            let mut value = json!({"type":"reasoning","id":id,"encrypted_content":"opaque"});
            if let Some(tag) = tag {
                value[CODEX_ORIGIN_FIELD] = json!(tag);
            }
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value,
            }
        };
        let ids = [
            format!("call_same|fc_same|{current}"),
            format!("call_other_account|fc_other_account|{other_account}"),
            format!("call_other_model|fc_other_model|{other_model}"),
            "call_unknown|fc_unknown".to_owned(),
        ];
        let mut messages = vec![crate::model::Message {
            role: Role::Assistant,
            content: vec![
                reasoning(Some(&current), "rs_same"),
                reasoning(Some(&other_account), "rs_other_account"),
                reasoning(Some(&other_model), "rs_other_model"),
                reasoning(None, "rs_unknown"),
                Content::Text {
                    text: "Public answer".into(),
                },
            ],
        }];
        for id in &ids {
            messages.push(crate::model::Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: id.clone(),
                    name: "Read".into(),
                    input: json!({"file_path":"note.txt"}),
                }],
            });
            messages.push(crate::model::Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: id.clone(),
                    content: "Public result".into(),
                    is_error: false,
                }],
            });
        }
        let original = messages.clone();
        let body = encode_request(
            &config,
            ModelRequest {
                history_sources: Vec::new(),
                messages,
                tools: Vec::new(),
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            &current,
        )
        .unwrap();
        assert_eq!(original[0].content.len(), 5);
        let items = body["input"].as_array().unwrap();
        let reasoning_items = items
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .collect::<Vec<_>>();
        assert_eq!(reasoning_items.len(), 1);
        assert_eq!(reasoning_items[0]["id"], "rs_same");
        assert!(reasoning_items[0].get(CODEX_ORIGIN_FIELD).is_none());
        assert!(items.iter().any(|item| item["content"] == "Public answer"));
        let calls = items
            .iter()
            .filter(|item| item["type"] == "function_call")
            .collect::<Vec<_>>();
        let results = items
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 4);
        assert_eq!(results.len(), 4);
        assert_eq!(calls[0]["id"], "fc_same");
        assert!(calls[1..].iter().all(|item| item.get("id").is_none()));
        for (call, result) in calls.iter().zip(results) {
            assert_eq!(call["call_id"], result["call_id"]);
        }
    }

    fn event(kind: &str, sequence: u64, value: Value) -> String {
        let mut value = value;
        value["type"] = json!(kind);
        value["sequence_number"] = json!(sequence);
        format!("event: {kind}\ndata: {value}\n\n")
    }

    pub(super) fn state_with_credentials() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = directory.path().canonicalize().unwrap();
        let auth_dir = state_dir.join("auth");
        fs::create_dir(&auth_dir).unwrap();
        fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let expires_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let credential_path = auth_dir.join("openai-codex-credentials.json");
        fs::write(
            &credential_path,
            json!({
                "access_token":"local-test-access-token",
                "refresh_token":"local-test-refresh-token",
                "account_id":"local-test-account",
                "expires_at_ms":expires_at_ms,
            })
            .to_string(),
        )
        .unwrap();
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600)).unwrap();
        (directory, state_dir)
    }

    pub(super) fn text_stream() -> String {
        let reasoning = json!({"type":"reasoning","id":"rs-1","summary":[{"type":"summary_text","text":"Considering"}],"encrypted_content":"opaque"});
        let text = json!({"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello from Codex","annotations":[]}]});
        [
            event("response.created", 0, json!({"response":{"id":"resp-1","status":"in_progress"}})),
            event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"reasoning","id":"rs-1","summary":[]}})),
            event("response.reasoning_summary_text.delta", 2, json!({"output_index":0,"item_id":"rs-1","delta":"Considering"})),
            event("response.output_item.done", 3, json!({"output_index":0,"item":reasoning})),
            event("response.output_item.added", 4, json!({"output_index":1,"item":{"type":"message","id":"msg-1","role":"assistant","status":"in_progress","content":[]}})),
            event("response.output_text.delta", 5, json!({"output_index":1,"item_id":"msg-1","content_index":0,"delta":"Hello from Codex"})),
            event("response.output_item.done", 6, json!({"output_index":1,"item":text})),
            event("response.done", 7, json!({"response":{"id":"resp-1","status":"completed","output":[reasoning,text],"usage":{"input_tokens":25,"output_tokens":4}}})),
        ].concat()
    }

    #[test]
    fn test_endpoint_accepts_only_numeric_loopback_codex_responses() {
        let directory = tempfile::tempdir().unwrap();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        for endpoint in [
            "http://localhost:1234/codex/responses",
            "https://127.0.0.1:1234/codex/responses",
            "http://192.0.2.1:1234/codex/responses",
            "http://127.0.0.1:1234/other",
            "http://user@127.0.0.1:1234/codex/responses",
            "http://127.0.0.1:1234/codex/responses?token=1",
        ] {
            let client = CodexModelClient::new(config.clone(), directory.path()).unwrap();
            assert!(
                client.with_loopback_test_endpoint(endpoint).is_err(),
                "{endpoint}"
            );
        }
        let client = CodexModelClient::new(config, directory.path()).unwrap();
        assert!(
            client
                .with_loopback_test_endpoint("http://127.0.0.1:1234/codex/responses")
                .is_ok()
        );
    }

    async fn serve_codex_response() -> (Url, oneshot::Receiver<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/codex/responses",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, request) = read_request(&mut socket).await;
            sender.send((headers, request)).unwrap();
            let stream = text_stream();
            send_stream(&mut socket, 200, &stream).await;
        });
        (endpoint, receiver)
    }

    async fn read_request(socket: &mut TcpStream) -> (String, Value) {
        let (headers, body) = read_raw_request(socket).await;
        (headers, serde_json::from_slice(&body).unwrap())
    }

    async fn read_raw_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        let (headers, body_start, body_length) = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            received.extend_from_slice(&buffer[..count]);
            if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8(received[..end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (headers, end + 4, length);
            }
        };
        while received.len() < body_start + body_length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            received.extend_from_slice(&buffer[..count]);
        }
        (
            headers,
            received[body_start..body_start + body_length].to_vec(),
        )
    }

    async fn send_stream(socket: &mut TcpStream, status: u16, body: &str) {
        socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    }

    fn tool_stream() -> String {
        let mut events = vec![event(
            "response.created",
            0,
            json!({"response":{"id":"resp-tools","status":"in_progress"}}),
        )];
        let mut output = Vec::new();
        let mut sequence = 1;
        for (index, file) in ["first.txt", "second.txt"].iter().enumerate() {
            let id = format!("fc_{}", index + 1);
            let call_id = format!("call_{}", index + 1);
            let arguments = format!("{{\"file_path\":\"{file}\"}}");
            events.push(event("response.output_item.added", sequence, json!({"output_index":index,"item":{"type":"function_call","id":id,"call_id":call_id,"name":"Read","arguments":""}})));
            sequence += 1;
            for fragment in ["{\"file_path\":\"", *file, "\"}"] {
                events.push(event(
                    "response.function_call_arguments.delta",
                    sequence,
                    json!({"output_index":index,"item_id":id,"delta":fragment}),
                ));
                sequence += 1;
            }
            events.push(event(
                "response.function_call_arguments.done",
                sequence,
                json!({"output_index":index,"item_id":id,"arguments":arguments}),
            ));
            sequence += 1;
            let item = json!({"type":"function_call","id":id,"call_id":call_id,"name":"Read","arguments":arguments,"status":"completed"});
            events.push(event(
                "response.output_item.done",
                sequence,
                json!({"output_index":index,"item":item}),
            ));
            sequence += 1;
            output.push(item);
        }
        events.push(event("response.done", sequence, json!({"response":{"id":"resp-tools","status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":8}}})));
        events.concat()
    }

    pub(super) fn reply_stream(response_id: &str, text: &str, input_tokens: u64) -> String {
        let item = json!({"type":"message","id":format!("msg-{response_id}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
        [
            event("response.created", 0, json!({"response":{"id":response_id,"status":"in_progress"}})),
            event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"message","id":format!("msg-{response_id}"),"role":"assistant","status":"in_progress","content":[]}})),
            event("response.output_text.delta", 2, json!({"output_index":0,"item_id":format!("msg-{response_id}"),"content_index":0,"delta":text})),
            event("response.output_item.done", 3, json!({"output_index":0,"item":item})),
            event("response.done", 4, json!({"response":{"id":response_id,"status":"completed","output":[item],"usage":{"input_tokens":input_tokens,"output_tokens":6}}})),
        ].concat()
    }

    pub(super) fn function_call_stream(
        response_id: &str,
        name: &str,
        arguments: Value,
        input_tokens: u64,
    ) -> String {
        let arguments = arguments.to_string();
        let item = json!({"type":"function_call","id":format!("fc_{response_id}"),"call_id":format!("call_{response_id}"),"name":name,"arguments":arguments,"status":"completed"});
        [
            event("response.created", 0, json!({"response":{"id":response_id,"status":"in_progress"}})),
            event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"function_call","id":format!("fc_{response_id}"),"call_id":format!("call_{response_id}"),"name":name,"arguments":""}})),
            event("response.function_call_arguments.delta", 2, json!({"output_index":0,"item_id":format!("fc_{response_id}"),"delta":arguments})),
            event("response.function_call_arguments.done", 3, json!({"output_index":0,"item_id":format!("fc_{response_id}"),"arguments":arguments})),
            event("response.output_item.done", 4, json!({"output_index":0,"item":item})),
            event("response.done", 5, json!({"response":{"id":response_id,"status":"completed","output":[item],"usage":{"input_tokens":input_tokens,"output_tokens":3}}})),
        ].concat()
    }

    pub(super) async fn serve_scripted_responses(
        streams: Vec<(u16, String)>,
    ) -> (Url, oneshot::Receiver<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/codex/responses",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, stream) in streams {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (_, body) = read_request(&mut socket).await;
                requests.push(body);
                send_stream(&mut socket, status, &stream).await;
            }
            sender.send(requests).unwrap();
        });
        (endpoint, receiver)
    }

    async fn serve_requests_with_headers(
        streams: Vec<String>,
    ) -> (Url, oneshot::Receiver<Vec<(String, Value)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/codex/responses",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::new();
            for stream in streams {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                send_stream(&mut socket, 200, &stream).await;
            }
            sender.send(requests).unwrap();
        });
        (endpoint, receiver)
    }

    pub(super) fn rewrite_credentials(
        state_dir: &Path,
        access_token: &str,
        account_id: &str,
        ttl_ms: u64,
    ) {
        let credential_path = state_dir.join("auth/openai-codex-credentials.json");
        let expires_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + ttl_ms;
        fs::write(
            &credential_path,
            json!({
                "access_token":access_token,
                "refresh_token":"local-test-refresh-token",
                "account_id":account_id,
                "expires_at_ms":expires_at_ms,
            })
            .to_string(),
        )
        .unwrap();
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn simple_request() -> ModelRequest {
        ModelRequest {
            history_sources: Vec::new(),
            messages: vec![crate::model::Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "Hello".into(),
                }],
            }],
            tools: Vec::new(),
            structured_output: None,
            temperature: None,
            reasoning_effort: None,
        }
    }

    pub(super) async fn send_ws_stream(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        sse: &str,
    ) {
        for event in sse.split("\n\n") {
            if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                socket.send(WsMessage::Text(data.into())).await.unwrap();
            }
        }
    }

    async fn completed_agent_turn(
        events: &mut tokio::sync::broadcast::Receiver<crate::runtime::Event>,
    ) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(reason) => panic!("Codex WS Agent request failed: {reason}"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn agent_websocket_reuses_chat_connection_for_tools_and_continuation() {
        let (_directory, state_dir) = state_with_credentials();
        fs::write(state_dir.join("first.txt"), "FIRST_WS_MARKER").unwrap();
        fs::write(state_dir.join("second.txt"), "SECOND_WS_MARKER").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (captured_tx, captured_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut frames = Vec::new();
            for reply in [
                tool_stream(),
                reply_stream("ws-tools", "Files read", 30),
                reply_stream("ws-followup", "Both markers remain", 12),
            ] {
                let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
                frames.push(serde_json::from_str::<Value>(&frame).unwrap());
                send_ws_stream(&mut socket, &reply).await;
            }
            captured_tx.send(frames).unwrap();
        });
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        let first = agent
            .submit(chat, None, "Read both files".into())
            .await
            .unwrap();
        completed_agent_turn(&mut events).await;
        agent
            .submit(chat, Some(first.loop_id), "Repeat their markers".into())
            .await
            .unwrap();
        completed_agent_turn(&mut events).await;
        let frames = tokio::time::timeout(Duration::from_secs(5), captured_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["type"], "response.create");
        assert_eq!(frames[0]["prompt_cache_key"], chat.to_string());
        assert_eq!(frames[0]["store"], false);
        assert_eq!(frames[1]["previous_response_id"], "resp-tools");
        assert!(frames[2]["previous_response_id"].is_null());
        assert_ne!(frames[1]["instructions"], frames[2]["instructions"]);
        assert!(
            frames[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call_output")
        );
        assert!(frames[1]["input"].as_array().unwrap().iter().any(|item| {
            item["output"]
                .as_str()
                .is_some_and(|text| text.contains("FIRST_WS_MARKER"))
        }));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_agent_chats_keep_websocket_tools_and_results_separate() {
        let (_directory, state_dir) = state_with_credentials();
        fs::write(state_dir.join("parallel-a.txt"), "PARALLEL_A_MARKER").unwrap();
        fs::write(state_dir.join("parallel-b.txt"), "PARALLEL_B_MARKER").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let first_chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let second_chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (captured_tx, captured_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut connections = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let request: Value = serde_json::from_str(&frame).unwrap();
                let chat =
                    uuid::Uuid::parse_str(request["prompt_cache_key"].as_str().unwrap()).unwrap();
                assert!(chat == first_chat || chat == second_chat);
                connections.push((chat, socket, request));
            }
            ready_tx.send(()).unwrap();
            release_rx.await.unwrap();
            for (chat, socket, _) in &mut connections {
                let path = if *chat == first_chat {
                    "parallel-a.txt"
                } else {
                    "parallel-b.txt"
                };
                send_ws_stream(
                    socket,
                    &function_call_stream("parallel", "Read", json!({"file_path":path}), 8),
                )
                .await;
            }
            let mut captured = Vec::new();
            for (chat, mut socket, first) in connections {
                let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let continuation: Value = serde_json::from_str(&frame).unwrap();
                let answer = if chat == first_chat {
                    "A_DONE"
                } else {
                    "B_DONE"
                };
                send_ws_stream(&mut socket, &reply_stream("parallel-reply", answer, 12)).await;
                captured.push((chat, first, continuation));
            }
            captured_tx.send(captured).unwrap();
        });
        let mut events = agent.subscribe();
        agent
            .submit(first_chat, None, "Read parallel-a.txt".into())
            .await
            .unwrap();
        agent
            .submit(second_chat, None, "Read parallel-b.txt".into())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        release_tx.send(()).unwrap();
        let mut completed = HashSet::new();
        let mut tool_completed = HashSet::new();
        while completed.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event.kind {
                EventKind::ToolCompleted { .. } => {
                    tool_completed.insert(event.chat_id);
                }
                EventKind::Completed { .. } => {
                    completed.insert(event.chat_id);
                }
                EventKind::Failed(reason) => panic!("concurrent Codex Chat failed: {reason}"),
                _ => {}
            }
        }
        assert_eq!(tool_completed, HashSet::from([first_chat, second_chat]));
        let captured = captured_rx.await.unwrap();
        assert_eq!(captured.len(), 2);
        for (chat, first, continuation) in captured {
            let (own, foreign) = if chat == first_chat {
                ("PARALLEL_A_MARKER", "PARALLEL_B_MARKER")
            } else {
                ("PARALLEL_B_MARKER", "PARALLEL_A_MARKER")
            };
            assert_eq!(first["prompt_cache_key"], chat.to_string());
            assert!(first["previous_response_id"].is_null());
            assert_eq!(continuation["prompt_cache_key"], chat.to_string());
            let results = continuation["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 1);
            let output = results[0]["output"].as_str().unwrap();
            assert!(output.contains(own));
            assert!(!output.contains(foreign));
            let snapshot = agent.snapshot(chat).await.unwrap();
            let text = snapshot
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(if chat == first_chat {
                "A_DONE"
            } else {
                "B_DONE"
            }));
            assert!(!text.contains(if chat == first_chat {
                "B_DONE"
            } else {
                "A_DONE"
            }));
        }
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_sends_only_new_input_when_context_is_identical() {
        let (_directory, state_dir) = state_with_credentials();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (captured_tx, captured_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut frames = Vec::new();
            for reply in [
                reply_stream("stable-one", "First", 10),
                reply_stream("stable-two", "Second", 8),
            ] {
                let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
                frames.push(serde_json::from_str::<Value>(&frame).unwrap());
                send_ws_stream(&mut socket, &reply).await;
            }
            captured_tx.send(frames).unwrap();
        });
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config, &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let chat = uuid::Uuid::new_v4();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let first = client
            .complete_for_chat(
                chat,
                simple_request(),
                events.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let second_request = ModelRequest {
            history_sources: Vec::new(),
            messages: vec![
                crate::model::Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Hello".into(),
                    }],
                },
                crate::model::Message {
                    role: Role::Assistant,
                    content: first.content,
                },
                crate::model::Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Again".into(),
                    }],
                },
            ],
            ..simple_request()
        };
        let second = client
            .complete_for_chat(chat, second_request, events, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            second
                .content
                .iter()
                .any(|item| matches!(item, Content::Text { text } if text == "Second"))
        );
        let frames = captured_rx.await.unwrap();
        assert!(frames[0]["previous_response_id"].is_null());
        assert_eq!(frames[1]["previous_response_id"], "stable-one");
        assert_eq!(frames[1]["input"].as_array().unwrap().len(), 1);
        assert_eq!(frames[1]["input"][0]["content"], "Again");
    }

    #[tokio::test]
    async fn agent_falls_back_to_sse_only_before_websocket_output() {
        let (_directory, state_dir) = state_with_credentials();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (captured_tx, captured_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let request = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            send_ws_stream(
                &mut websocket,
                &event(
                    "response.created",
                    0,
                    json!({"response":{"id":"before-output","status":"in_progress"}}),
                ),
            )
            .await;
            websocket.close(None).await.unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut socket).await;
            send_stream(
                &mut socket,
                200,
                &reply_stream("sse-fallback", "Recovered through SSE", 8),
            )
            .await;
            captured_tx
                .send((
                    serde_json::from_str::<Value>(&request).unwrap(),
                    headers,
                    body,
                ))
                .unwrap();
        });
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Hello".into()).await.unwrap();
        completed_agent_turn(&mut events).await;
        let (ws, headers, sse) = captured_rx.await.unwrap();
        assert_eq!(ws["type"], "response.create");
        assert_eq!(sse["input"][0]["content"], "Hello");
        assert!(headers.to_ascii_lowercase().contains("session-id: "));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn agent_never_replays_after_websocket_output_begins() {
        let (_directory, state_dir) = state_with_credentials();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (requests_tx, requests_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let _ = websocket.next().await.unwrap().unwrap();
            let partial = [
                event("response.created", 0, json!({"response":{"id":"partial","status":"in_progress"}})),
                event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"message","id":"msg-partial","role":"assistant","status":"in_progress","content":[]}})),
                event("response.output_text.delta", 2, json!({"output_index":0,"item_id":"msg-partial","content_index":0,"delta":"Partial"})),
            ].concat();
            send_ws_stream(&mut websocket, &partial).await;
            websocket.close(None).await.unwrap();
            let retried = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .is_ok();
            requests_tx.send(retried).unwrap();
        });
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Hello".into()).await.unwrap();
        let mut saw_partial = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Text(text) if text == "Partial" => saw_partial = true,
                EventKind::Failed(reason) => {
                    assert!(reason.contains("after output began"));
                    break;
                }
                EventKind::Completed { .. } => panic!("partial WebSocket response completed"),
                _ => {}
            }
        }
        assert!(saw_partial);
        assert!(!requests_rx.await.unwrap());
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_retries_missing_continuation_once_with_full_history() {
        verify_missing_websocket_continuation(false).await;
    }

    #[tokio::test]
    async fn websocket_stops_when_full_history_cannot_recover_missing_continuation() {
        verify_missing_websocket_continuation(true).await;
    }

    async fn verify_missing_websocket_continuation(reject_full_history: bool) {
        let (_directory, state_dir) = state_with_credentials();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (captured_tx, captured_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let first = serde_json::from_str::<Value>(
                &socket.next().await.unwrap().unwrap().into_text().unwrap(),
            )
            .unwrap();
            send_ws_stream(&mut socket, &reply_stream("before-invalid", "First", 8)).await;
            let delta = serde_json::from_str::<Value>(
                &socket.next().await.unwrap().unwrap().into_text().unwrap(),
            )
            .unwrap();
            socket
                .send(WsMessage::Text(
                    json!({"type":"error","error":{"code":"previous_response_id_not_found"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let full = serde_json::from_str::<Value>(
                &socket.next().await.unwrap().unwrap().into_text().unwrap(),
            )
            .unwrap();
            if reject_full_history {
                socket
                    .send(WsMessage::Text(
                        json!({"type":"error","error":{"code":"previous_response_id_not_found"}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            } else {
                send_ws_stream(&mut socket, &reply_stream("after-invalid", "Second", 9)).await;
            }
            captured_tx.send((first, delta, full)).unwrap();
        });
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config, &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let chat = uuid::Uuid::new_v4();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let first = client
            .complete_for_chat(
                chat,
                simple_request(),
                events.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let request = ModelRequest {
            history_sources: Vec::new(),
            messages: vec![
                crate::model::Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Hello".into(),
                    }],
                },
                crate::model::Message {
                    role: Role::Assistant,
                    content: first.content,
                },
                crate::model::Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Again".into(),
                    }],
                },
            ],
            ..simple_request()
        };
        let result = client
            .complete_for_chat(chat, request, events, CancellationToken::new())
            .await;
        if reject_full_history {
            assert!(matches!(
                result,
                Err(ModelError::ResponseRejected(
                    ModelResponseRejection::UnsupportedContinuation
                ))
            ));
        } else {
            assert!(
                result
                    .unwrap()
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Text { text } if text == "Second"))
            );
        }
        let (first, delta, full) = captured_rx.await.unwrap();
        assert!(first["previous_response_id"].is_null());
        assert_eq!(delta["previous_response_id"], "before-invalid");
        assert_eq!(delta["input"].as_array().unwrap().len(), 1);
        assert!(full["previous_response_id"].is_null());
        assert_eq!(full["input"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn websocket_connections_are_separate_for_chats_and_accounts() {
        let (_directory, state_dir) = state_with_credentials();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (captured_tx, captured_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut connections = Vec::new();
            let mut frames = Vec::new();
            let mut authorizations = Vec::new();
            for index in 0..4 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut authorization = String::new();
                let mut socket =
                    accept_hdr_async(stream, |request: &WsRequest, response: WsResponse| {
                        authorization = request
                            .headers()
                            .get("authorization")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        Ok(response)
                    })
                    .await
                    .unwrap();
                authorizations.push(authorization);
                let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
                frames.push(serde_json::from_str::<Value>(&frame).unwrap());
                send_ws_stream(
                    &mut socket,
                    &reply_stream(&format!("isolation-{index}"), "Done", 4),
                )
                .await;
                connections.push(socket);
            }
            captured_tx.send((frames, authorizations)).unwrap();
            let _ = connections;
        });
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config, &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap()
            .with_loopback_test_websocket();
        let first_chat = uuid::Uuid::new_v4();
        let second_chat = uuid::Uuid::new_v4();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        client
            .complete_for_chat(
                first_chat,
                simple_request(),
                events.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        client
            .complete_for_chat(
                second_chat,
                simple_request(),
                events.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        rewrite_credentials(
            &state_dir,
            "same-account-new-token",
            "local-test-account",
            3_600_000,
        );
        client
            .complete_for_chat(
                first_chat,
                simple_request(),
                events.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        rewrite_credentials(&state_dir, "new-account-token", "new-account", 3_600_000);
        client
            .complete_for_chat(
                first_chat,
                simple_request(),
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (frames, authorizations) = tokio::time::timeout(Duration::from_secs(5), captured_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0]["prompt_cache_key"], first_chat.to_string());
        assert_eq!(frames[1]["prompt_cache_key"], second_chat.to_string());
        assert_eq!(frames[2]["prompt_cache_key"], first_chat.to_string());
        assert_eq!(frames[3]["prompt_cache_key"], first_chat.to_string());
        assert!(
            frames
                .iter()
                .all(|frame| frame["previous_response_id"].is_null())
        );
        assert_eq!(authorizations[0], "Bearer local-test-access-token");
        assert_eq!(authorizations[1], "Bearer local-test-access-token");
        assert_eq!(authorizations[2], "Bearer same-account-new-token");
        assert_eq!(authorizations[3], "Bearer new-account-token");
    }

    #[tokio::test]
    async fn codex_incomplete_and_malformed_tool_streams_never_complete() {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let incomplete = [
            event("response.created", 0, json!({"response":{"id":"resp-incomplete","status":"in_progress"}})),
            event("response.done", 1, json!({"response":{"id":"resp-incomplete","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}})),
        ].concat();
        let malformed = [
            event("response.created", 0, json!({"response":{"id":"resp-malformed","status":"in_progress"}})),
            event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"function_call","id":"fc_bad","call_id":"call_bad","name":"Read","arguments":""}})),
            event("response.function_call_arguments.delta", 2, json!({"output_index":0,"item_id":"fc_bad","delta":"{\"file_path\":"})),
            event("response.function_call_arguments.done", 3, json!({"output_index":0,"item_id":"fc_bad","arguments":"{\"file_path\":\"secret\"}"})),
        ].concat();
        let (endpoint, captured) =
            serve_scripted_responses(vec![(200, incomplete), (200, malformed)]).await;
        let client = CodexModelClient::new(config, &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap();
        for expected in ["output limit", "invalid"] {
            let (events, _) = tokio::sync::mpsc::unbounded_channel();
            let error = client
                .complete(simple_request(), events, CancellationToken::new())
                .await
                .unwrap_err();
            match expected {
                "output limit" => assert!(matches!(
                    error,
                    ModelError::ResponseRejected(ModelResponseRejection::OutputLimit)
                )),
                _ => assert!(matches!(error, ModelError::InvalidResponse(_))),
            }
        }
        assert_eq!(captured.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn codex_sse_interruption_after_reasoning_is_terminal() {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let partial = [
            event("response.created", 0, json!({"response":{"id":"sse-partial","status":"in_progress"}})),
            event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"reasoning","id":"rs-partial","summary":[]}})),
            event("response.reasoning_summary_text.delta", 2, json!({"output_index":0,"item_id":"rs-partial","delta":"Thinking"})),
        ].concat();
        let (endpoint, captured) = serve_scripted_responses(vec![(200, partial)]).await;
        let client = CodexModelClient::new(config, &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap();
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let error = client
            .complete(simple_request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ModelError::StreamInterruptedAfterOutput));
        assert!(
            matches!(receiver.try_recv(), Ok(ModelDelta::Thinking(text)) if text == "Thinking")
        );
        assert_eq!(captured.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelling_codex_stream_stops_the_request_without_a_terminal_event() {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let (started, received) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
            socket
                .write_all(
                    event(
                        "response.created",
                        0,
                        json!({"response":{"id":"resp-cancel","status":"in_progress"}}),
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            started.send(()).unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let client = Arc::new(
            CodexModelClient::new(config, &state_dir)
                .unwrap()
                .with_loopback_test_endpoint(&endpoint)
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let run = tokio::spawn(async move {
            let (events, _) = tokio::sync::mpsc::unbounded_channel();
            client
                .complete(simple_request(), events, request_cancel)
                .await
        });
        received.await.unwrap();
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), run)
                .await
                .unwrap()
                .unwrap(),
            Err(ModelError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn agent_conversation_uses_codex_credentials_and_streams_text_and_reasoning() {
        let (_directory, state_dir) = state_with_credentials();

        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let (endpoint, captured) = serve_codex_response().await;
        let mut client = CodexModelClient::new(config.clone(), &state_dir).unwrap();
        client.endpoint = endpoint;
        client.client = reqwest::Client::builder().no_proxy().build().unwrap();
        client.prefer_websocket = false;
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Say hello".into()).await.unwrap();
        let mut text = String::new();
        let mut thinking = String::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event.kind {
                EventKind::Text(delta) => text.push_str(&delta),
                EventKind::Thinking(delta) => thinking.push_str(&delta),
                EventKind::Completed { usage } => {
                    assert_eq!(usage.input_tokens, Some(25));
                    break;
                }
                EventKind::Failed(reason) => panic!("Agent request failed: {reason}"),
                _ => {}
            }
        }
        assert_eq!(text, "Hello from Codex");
        assert_eq!(thinking, "Considering");
        let (headers, body) = captured.await.unwrap();
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with("post /codex/responses http/1.1"));
        assert!(headers.contains("authorization: bearer local-test-access-token"));
        assert!(headers.contains("chatgpt-account-id: local-test-account"));
        assert!(headers.contains("originator: tickloop"));
        assert!(headers.contains("openai-beta: responses=experimental"));
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["content"], "Say hello");
        assert!(
            body["instructions"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(body["max_output_tokens"].is_null());
        assert!(
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        );
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(
            snapshot
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| matches!(
                    content,
                    Content::ProtocolData {
                        protocol: Protocol::Responses,
                        ..
                    }
                ))
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn agent_uses_replaced_account_on_its_next_real_model_request() {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let (endpoint, captured) = serve_requests_with_headers(vec![
            text_stream(),
            reply_stream("after-account-switch", "New account", 30),
        ])
        .await;
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        let first = agent.submit(chat, None, "Hello".into()).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(reason) => panic!("first request failed: {reason}"),
                _ => {}
            }
        }
        rewrite_credentials(
            &state_dir,
            "local-test-access-token-2",
            "local-test-account-2",
            3_600_000,
        );
        agent
            .submit(chat, Some(first.loop_id), "Continue".into())
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(reason) => panic!("account replacement failed: {reason}"),
                _ => {}
            }
        }
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .0
                .to_ascii_lowercase()
                .contains("authorization: bearer local-test-access-token\r\n")
        );
        assert!(
            requests[0]
                .0
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: local-test-account\r\n")
        );
        assert!(
            requests[1]
                .0
                .to_ascii_lowercase()
                .contains("authorization: bearer local-test-access-token-2\r\n")
        );
        assert!(
            requests[1]
                .0
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: local-test-account-2\r\n")
        );
        assert!(
            requests[1].1["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "reasoning")
        );
        assert!(
            requests[1].1["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["content"] == "Hello from Codex")
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn agent_sends_refreshed_token_and_account_to_the_model_service() {
        const REFRESHED_TOKEN: &str = "ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1uZXcifX0.sig";
        let (_directory, state_dir) = state_with_credentials();
        rewrite_credentials(&state_dir, "expired-token", "old-account", 1_000);
        let oauth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let oauth_origin = format!("http://{}", oauth_listener.local_addr().unwrap());
        let (oauth_request_sender, oauth_request_receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = oauth_listener.accept().await.unwrap();
            let (headers, form) = read_raw_request(&mut socket).await;
            oauth_request_sender
                .send((headers, String::from_utf8(form).unwrap()))
                .unwrap();
            let response = json!({
                "access_token":REFRESHED_TOKEN,
                "refresh_token":"new-refresh-token",
                "expires_in":3600,
            })
            .to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        });
        let (endpoint, captured) = serve_requests_with_headers(vec![reply_stream(
            "after-refresh",
            "Fresh credentials",
            9,
        )])
        .await;
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap()
            .with_loopback_test_auth(&state_dir, &oauth_origin)
            .unwrap();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Hello".into()).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(reason) => panic!("refreshed Agent request failed: {reason}"),
                _ => {}
            }
        }
        let (oauth_headers, oauth_form) = oauth_request_receiver.await.unwrap();
        assert!(oauth_headers.starts_with("POST /oauth/token HTTP/1.1"));
        assert!(oauth_form.contains("grant_type=refresh_token"));
        assert!(oauth_form.contains("refresh_token=local-test-refresh-token"));
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 1);
        let headers = requests[0].0.to_ascii_lowercase();
        assert!(headers.contains(
            &format!("authorization: bearer {REFRESHED_TOKEN}\r\n").to_ascii_lowercase()
        ));
        assert!(headers.contains("chatgpt-account-id: acct-new\r\n"));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn expired_credentials_cannot_refresh_outside_the_shared_request_budget() {
        let (_directory, state_dir) = state_with_credentials();
        rewrite_credentials(&state_dir, "expired-token", "local-test-account", 1_000);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = format!("{origin}/codex/responses");
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let client = CodexModelClient::new(config, &state_dir)
            .unwrap()
            .with_loopback_test_auth(&state_dir, &origin)
            .unwrap()
            .with_loopback_test_endpoint(&endpoint)
            .unwrap();
        let budget = ModelRequestContext::new(1);
        assert!(budget.try_begin_attempt());
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let error = client
            .complete_for_chat_with_context(
                uuid::Uuid::new_v4(),
                simple_request(),
                events,
                CancellationToken::new(),
                budget.clone(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ModelError::RequestBudgetExhausted {
                stage: ModelRequestStage::Credentials
            }
        ));
        assert_eq!(budget.used_attempts(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn agent_reads_two_real_files_and_continues_with_paired_codex_tool_history() {
        let (_directory, state_dir) = state_with_credentials();
        fs::write(state_dir.join("first.txt"), "FRIDAY_MARKER_123").unwrap();
        fs::write(state_dir.join("second.txt"), "MONDAY_MARKER_456").unwrap();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let (endpoint, captured) = serve_scripted_responses(vec![
            (200, tool_stream()),
            (200, reply_stream("reply-one", "Friday and Monday", 30)),
            (
                200,
                reply_stream("reply-two", "The same dates are Friday and Monday", 44),
            ),
        ])
        .await;
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap();
        let agent = Agent::new_embedded(config, Arc::new(client));
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        let mut events = agent.subscribe();
        let first = agent
            .submit(chat, None, "Read both files and tell me their dates".into())
            .await
            .unwrap();
        let mut tool_completions = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::ToolCompleted { .. } => tool_completions += 1,
                EventKind::Completed { usage } => {
                    assert_eq!(usage.input_tokens, Some(42));
                    break;
                }
                EventKind::Failed(reason) => panic!("first Codex request failed: {reason}"),
                _ => {}
            }
        }
        assert_eq!(tool_completions, 2);
        agent
            .submit(chat, Some(first.loop_id), "What were the dates?".into())
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(reason) => panic!("followup Codex request failed: {reason}"),
                _ => {}
            }
        }
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 3);
        let tool_history = requests[1]["input"].as_array().unwrap();
        let calls = tool_history
            .iter()
            .filter(|item| item["type"] == "function_call")
            .collect::<Vec<_>>();
        let results = tool_history
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(results.len(), 2);
        for index in 0..2 {
            assert_eq!(calls[index]["call_id"], results[index]["call_id"]);
            assert_eq!(calls[index]["id"], format!("fc_{}", index + 1));
        }
        assert!(
            results[0]["output"]
                .as_str()
                .unwrap()
                .contains("FRIDAY_MARKER_123")
        );
        assert!(
            results[1]["output"]
                .as_str()
                .unwrap()
                .contains("MONDAY_MARKER_456")
        );
        assert!(requests[2]["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "function_call_output"
                && item["output"]
                    .as_str()
                    .unwrap()
                    .contains("FRIDAY_MARKER_123")
        }));
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text == "The same dates are Friday and Monday")));
        agent.shutdown().await;
    }

    fn approval_findings(block: bool) -> Value {
        let rules = [
            "external_code",
            "destruction",
            "persistence",
            "security",
            "exfiltration",
            "credentials",
            "external_scope",
            "finance",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            json!({
                "id": format!("builtin.soft_deny.{id}"),
                "applicability": if block && index == 0 { "present" } else { "absent" },
                "clearance": "missing"
            })
        })
        .collect::<Vec<_>>();
        json!({"task_scope":"within_task", "rules":rules})
    }

    #[tokio::test]
    async fn codex_auto_review_allows_or_blocks_a_real_protected_operation() {
        for should_block in [false, true] {
            let (_directory, state_dir) = state_with_credentials();
            let config = ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .find(|profile| {
                    profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
                })
                .unwrap();
            let (endpoint, captured) = serve_scripted_responses(vec![
                (
                    200,
                    function_call_stream(
                        "propose",
                        "Bash",
                        json!({"command":"ln -s missing-target checked"}),
                        12,
                    ),
                ),
                (
                    200,
                    function_call_stream(
                        "review",
                        "classify_result",
                        approval_findings(should_block),
                        7,
                    ),
                ),
                (200, reply_stream("after-review", "reviewed", 30)),
            ])
            .await;
            let client = CodexModelClient::new(config.clone(), &state_dir)
                .unwrap()
                .with_loopback_test_endpoint(endpoint.as_str())
                .unwrap();
            let client = Arc::new(client);
            let agent = Agent::new_embedded(config, client.clone());
            agent.set_auto_reviewer(client).await;
            let chat = agent.create_chat(state_dir.clone()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent
                .submit(
                    chat,
                    None,
                    "Create the checked link in this temporary directory".into(),
                )
                .await
                .unwrap();
            let mut decision = None;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .kind
                {
                    EventKind::PermissionChecked {
                        decision: checked, ..
                    } => decision = Some(checked),
                    EventKind::Completed { usage } => {
                        assert_eq!(usage.input_tokens, Some(42));
                        break;
                    }
                    EventKind::PermissionRequested { .. } => {
                        panic!("Codex Auto reviewer asked for user input")
                    }
                    EventKind::Failed(reason) => panic!("Codex Auto run failed: {reason}"),
                    _ => {}
                }
            }
            assert_eq!(
                decision,
                Some(if should_block {
                    Decision::Deny
                } else {
                    Decision::Allow
                })
            );
            assert_eq!(
                state_dir.join("checked").symlink_metadata().is_ok(),
                !should_block
            );
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1]["tool_choice"], "required");
            assert_eq!(requests[1]["parallel_tool_calls"], false);
            assert_eq!(requests[1]["tools"].as_array().unwrap().len(), 1);
            assert_eq!(requests[1]["tools"][0]["name"], "classify_result");
            assert_eq!(requests[1]["tools"][0]["strict"], true);
            assert!(!requests[1]["tools"].to_string().contains("Bash"));
            let snapshot = agent.snapshot(chat).await.unwrap();
            let usage = snapshot.request_usage.entries();
            assert_eq!(usage.len(), 3);
            assert_eq!(usage[1].kind, RequestKind::PermissionCheck);
            assert_eq!(usage[1].usage.input_tokens, Some(7));
            agent.shutdown().await;
        }
    }

    #[tokio::test]
    async fn explicit_codex_reviewer_uses_its_own_model_and_usage_identity() {
        let (_directory, state_dir) = state_with_credentials();
        let profiles = ModelCatalog::defaults().profiles();
        let main_config = profiles
            .iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap()
            .clone();
        let reviewer_config = profiles
            .iter()
            .find(|profile| {
                profile.model == "gpt-5.6-sol" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap()
            .clone();
        let (main_endpoint, main_requests) = serve_scripted_responses(vec![
            (
                200,
                function_call_stream(
                    "propose",
                    "Bash",
                    json!({"command":"ln -s missing-target approved"}),
                    12,
                ),
            ),
            (200, reply_stream("after-review", "approved", 18)),
        ])
        .await;
        let (reviewer_endpoint, reviewer_requests) = serve_scripted_responses(vec![(
            200,
            function_call_stream("review", "classify_result", approval_findings(false), 7),
        )])
        .await;
        let main_client = CodexModelClient::new(main_config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(main_endpoint.as_str())
            .unwrap();
        let reviewer_client = CodexModelClient::new(reviewer_config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(reviewer_endpoint.as_str())
            .unwrap();
        let agent = Agent::new_embedded(main_config.clone(), Arc::new(main_client));
        agent.set_auto_reviewer(Arc::new(reviewer_client)).await;
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(
                chat,
                None,
                "Create the approved link in this temporary directory".into(),
            )
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(reason) => panic!("explicit Codex reviewer failed: {reason}"),
                _ => {}
            }
        }
        assert!(state_dir.join("approved").symlink_metadata().is_ok());
        let main_requests = main_requests.await.unwrap();
        let reviewer_requests = reviewer_requests.await.unwrap();
        assert_eq!(main_requests.len(), 2);
        assert_eq!(reviewer_requests.len(), 1);
        assert_eq!(main_requests[0]["model"], "gpt-5.5");
        assert_eq!(reviewer_requests[0]["model"], "gpt-5.6-sol");
        assert_eq!(reviewer_requests[0]["tool_choice"], "required");
        let snapshot = agent.snapshot(chat).await.unwrap();
        let usage = snapshot.request_usage.entries();
        assert_eq!(usage.len(), 3);
        assert_eq!(usage[1].kind, RequestKind::PermissionCheck);
        assert_eq!(usage[1].model, reviewer_config.identity());
        assert_eq!(usage[1].usage.input_tokens, Some(7));
        agent.shutdown().await;
    }

    pub(super) async fn blocked_auto_review_case(
        review_responses: Vec<(u16, String)>,
    ) -> (Vec<Value>, Vec<RequestKind>, String) {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let mut responses = vec![(
            200,
            function_call_stream(
                "propose",
                "Bash",
                json!({"command":"ln -s missing-target blocked"}),
                12,
            ),
        )];
        responses.extend(review_responses);
        responses.push((
            200,
            reply_stream("after-block", "I will continue without the link", 18),
        ));
        let (endpoint, captured) = serve_scripted_responses(responses).await;
        let client = CodexModelClient::new(config.clone(), &state_dir)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap();
        let client = Arc::new(client);
        let agent = Agent::new_with_options(
            config,
            client.clone(),
            crate::runtime::AgentHostOptions {
                prompt_policy: crate::runtime::PromptPolicy::DenyOnPrompt,
                ..crate::runtime::AgentHostOptions::embedded_defaults()
            },
        );
        agent.set_auto_reviewer(client).await;
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(
                chat,
                None,
                "Inspect the directory without making changes".into(),
            )
            .await
            .unwrap();
        let mut reason = None;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::PermissionChecked {
                    decision,
                    reason: checked_reason,
                } => {
                    assert_eq!(decision, Decision::Deny);
                    reason = Some(checked_reason);
                }
                EventKind::Completed { .. } => break,
                EventKind::PermissionRequested { .. } => panic!("Auto review asked for user input"),
                EventKind::ToolStarted { .. } => panic!("Auto review admitted blocked operation"),
                EventKind::Failed(error) => panic!("Auto review failed the run: {error}"),
                _ => {}
            }
        }
        assert!(state_dir.join("blocked").symlink_metadata().is_err());
        let kinds = agent
            .snapshot(chat)
            .await
            .unwrap()
            .request_usage
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect();
        let requests = captured.await.unwrap();
        agent.shutdown().await;
        (requests, kinds, reason.unwrap())
    }

    #[tokio::test]
    async fn malformed_codex_review_is_retried_then_blocks_without_running_the_tool() {
        let malformed = || {
            function_call_stream(
                "bad-review",
                "classify_result",
                json!({"thinking":"fixture","shouldBlock":"false","reason":"wrong type"}),
                7,
            )
        };
        let (requests, kinds, reason) =
            blocked_auto_review_case(vec![(200, malformed()), (200, malformed())]).await;
        assert_eq!(requests.len(), 4);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == RequestKind::PermissionCheck)
                .count(),
            2
        );
        assert!(reason.to_ascii_lowercase().contains("invalid"), "{reason}");
    }

    #[tokio::test]
    async fn codex_review_rejects_extra_business_tools_before_execution() {
        let (requests, kinds, reason) = blocked_auto_review_case(vec![(200, tool_stream())]).await;
        assert_eq!(requests.len(), 3);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == RequestKind::PermissionCheck)
                .count(),
            1
        );
        assert!(reason.contains("unexpected tool operations"), "{reason}");
    }

    #[test]
    fn mixed_structured_output_always_rejects_unexpected_tools_before_format_errors() {
        let text = Content::Text {
            text: "valid JSON or commentary".into(),
        };
        let result = Content::ToolUse {
            id: "result".into(),
            name: "classify_result".into(),
            input: json!({}),
        };
        let operation = Content::ToolUse {
            id: "operation".into(),
            name: "Read".into(),
            input: json!({"file_path":"notes.txt"}),
        };
        for content in [
            vec![text.clone(), operation.clone()],
            vec![result.clone(), text.clone(), operation.clone()],
            vec![operation.clone(), text.clone(), result.clone()],
            vec![text.clone(), result.clone(), result.clone()],
        ] {
            for stop_reason in ["tool_use", "end_turn"] {
                let output = ModelOutput {
                    private_source: None,
                    content: content.clone(),
                    usage: Default::default(),
                    stop_reason: stop_reason.into(),
                };
                assert!(matches!(
                    normalize_structured_output(output, "classify_result"),
                    Err(ModelError::ResponseRejected(
                        ModelResponseRejection::UnexpectedToolOperation
                    ))
                ));
            }
        }
    }

    #[tokio::test]
    async fn structured_sse_and_websocket_reject_mixed_malformed_business_calls() {
        for websocket in [false, true] {
            let (_directory, state_dir) = state_with_credentials();
            let text = json!({"type":"message","id":"text","role":"assistant","status":"completed","content":[{"type":"output_text","text":"{\"task_scope\":\"within_task\",\"rules\":[]}"}]});
            let operation = json!({"type":"function_call","id":"operation","call_id":"unexpected","name":"Bash","arguments":"{truncated","status":"completed"});
            let response = [
                event("response.created", 0, json!({"response":{"id":"mixed","status":"in_progress"}})),
                event("response.output_item.added", 1, json!({"output_index":0,"item":text})),
                event("response.output_item.done", 2, json!({"output_index":0,"item":text})),
                event("response.output_item.added", 3, json!({"output_index":1,"item":operation})),
                event("response.output_item.done", 4, json!({"output_index":1,"item":operation})),
                event("response.done", 5, json!({"response":{"id":"mixed","status":"completed","output":[text,operation]}})),
            ].concat();
            let (endpoint, captured) = if websocket {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
                let (sender, captured) = oneshot::channel();
                tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut socket = accept_async(stream).await.unwrap();
                    let request = socket.next().await.unwrap().unwrap().into_text().unwrap();
                    for line in response
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                    {
                        if socket
                            .send(WsMessage::Text(line.to_owned().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    sender
                        .send(vec![serde_json::from_str::<Value>(&request).unwrap()])
                        .unwrap();
                });
                (endpoint, captured)
            } else {
                let (endpoint, captured) = serve_scripted_responses(vec![(200, response)]).await;
                (endpoint.to_string(), captured)
            };
            let config = ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .find(|profile| {
                    profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
                })
                .unwrap();
            let mut client = CodexModelClient::new(config, &state_dir)
                .unwrap()
                .with_loopback_test_endpoint(&endpoint)
                .unwrap();
            if websocket {
                client = client.with_loopback_test_websocket();
            }
            let mut request = simple_request();
            request.structured_output = Some(crate::model::StructuredOutput {
                name: "classify_result".into(),
                schema: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            });
            let (events, _) = tokio::sync::mpsc::unbounded_channel();
            assert!(
                matches!(
                    client
                        .complete(request, events, CancellationToken::new())
                        .await,
                    Err(ModelError::ResponseRejected(
                        ModelResponseRejection::UnexpectedToolOperation
                    ))
                ),
                "websocket={websocket}"
            );
            assert_eq!(captured.await.unwrap().len(), 1);
        }
    }

    async fn orphan_tool_event_requires_manual_review(malformed_result: bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for websocket in [false, true] {
            for event_type in [
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.function_call_output.delta",
                "response.mcp_call_arguments.delta",
                "response.file_search_call.searching",
                "response.code_interpreter_call_code.delta",
            ] {
                let (_directory, state_dir) = state_with_credentials();
                let config = ModelCatalog::defaults()
                    .profiles()
                    .into_iter()
                    .find(|profile| {
                        profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
                    })
                    .unwrap();
                let main_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let main_endpoint = format!(
                    "http://{}/codex/responses",
                    main_listener.local_addr().unwrap()
                );
                let main_server = tokio::spawn(async move {
                    for reply in [
                        function_call_stream(
                            "proposal",
                            "Bash",
                            json!({"command":"ln -s missing-target blocked"}),
                            12,
                        ),
                        reply_stream("done", "Done without the operation", 12),
                    ] {
                        let (mut socket, _) = main_listener.accept().await.unwrap();
                        read_request(&mut socket).await;
                        send_stream(&mut socket, 200, &reply).await;
                    }
                });
                let input = if malformed_result {
                    json!(true)
                } else {
                    approval_findings(false)
                };
                let original = function_call_stream("review", "classify_result", input, 7);
                let mut response = String::new();
                for (index, chunk) in original
                    .split("\n\n")
                    .filter(|chunk| !chunk.is_empty())
                    .enumerate()
                {
                    let mut data: Value = serde_json::from_str(
                        chunk
                            .lines()
                            .find_map(|line| line.strip_prefix("data: "))
                            .unwrap(),
                    )
                    .unwrap();
                    data["sequence_number"] = json!(if index > 1 { index + 1 } else { index });
                    response += &format!(
                        "event: {}\ndata: {data}\n\n",
                        data["type"].as_str().unwrap()
                    );
                    if index == 1 {
                        response += &event(
                            event_type,
                            2,
                            json!({"output_index":42,"item_id":"orphan","delta":"unexpected command","input":"unexpected command"}),
                        );
                    }
                }
                let repair =
                    function_call_stream("repair", "classify_result", approval_findings(false), 7);
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
                let calls = Arc::new(AtomicUsize::new(0));
                let captured = calls.clone();
                let server = tokio::spawn(async move {
                    if websocket {
                        let (stream, _) = listener.accept().await.unwrap();
                        let mut socket = accept_async(stream).await.unwrap();
                        for reply in [response, repair] {
                            let Some(Ok(WsMessage::Text(_))) = socket.next().await else {
                                break;
                            };
                            captured.fetch_add(1, Ordering::SeqCst);
                            for line in reply.lines().filter_map(|line| line.strip_prefix("data: "))
                            {
                                if socket
                                    .send(WsMessage::Text(line.to_owned().into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    } else {
                        for reply in [response, repair] {
                            let (mut socket, _) = listener.accept().await.unwrap();
                            read_request(&mut socket).await;
                            captured.fetch_add(1, Ordering::SeqCst);
                            send_stream(&mut socket, 200, &reply).await;
                        }
                    }
                });
                let main = CodexModelClient::new(config.clone(), &state_dir)
                    .unwrap()
                    .with_loopback_test_endpoint(main_endpoint.as_str())
                    .unwrap();
                let mut reviewer = CodexModelClient::new(config.clone(), &state_dir)
                    .unwrap()
                    .with_loopback_test_endpoint(&endpoint)
                    .unwrap();
                if websocket {
                    reviewer = reviewer.with_loopback_test_websocket();
                }
                let agent = Agent::new_embedded(config, Arc::new(main));
                agent.set_auto_reviewer(Arc::new(reviewer)).await;
                let chat = agent.create_chat(state_dir.clone()).await.unwrap();
                agent
                    .set_execution_mode(chat, ExecutionMode::Auto)
                    .await
                    .unwrap();
                let mut events = agent.subscribe();
                agent
                    .submit(chat, None, "Create the blocked link".into())
                    .await
                    .unwrap();
                let mut manual = false;
                loop {
                    match tokio::time::timeout(Duration::from_secs(5), events.recv())
                        .await
                        .unwrap()
                        .unwrap()
                        .kind
                    {
                        EventKind::PermissionChecked { decision, reason } => {
                            assert_eq!(
                                decision,
                                Decision::Ask,
                                "{event_type}, websocket={websocket}, malformed={malformed_result}: {reason}"
                            );
                            assert!(reason.contains("unexpected tool operations"), "{reason}");
                        }
                        EventKind::PermissionRequested { request } => {
                            manual = true;
                            agent
                                .answer_permission(chat, request.id, false)
                                .await
                                .unwrap();
                        }
                        EventKind::ToolStarted { .. } => {
                            panic!("orphan tool event admitted the original operation")
                        }
                        EventKind::Completed { .. } | EventKind::Interrupted => break,
                        EventKind::Failed(error) => panic!("{error}"),
                        _ => {}
                    }
                }
                assert!(manual);
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "the queued valid Allow must not be requested"
                );
                assert!(state_dir.join("blocked").symlink_metadata().is_err());
                assert_eq!(
                    agent
                        .snapshot(chat)
                        .await
                        .unwrap()
                        .request_usage
                        .entries()
                        .iter()
                        .filter(|entry| entry.kind == RequestKind::PermissionCheck)
                        .count(),
                    1
                );
                agent.shutdown().await;
                main_server.abort();
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn allowing_result_with_orphan_custom_tool_event_requires_manual_review() {
        orphan_tool_event_requires_manual_review(false).await;
    }

    #[tokio::test]
    async fn malformed_result_with_orphan_custom_tool_event_never_gets_a_correction_retry() {
        orphan_tool_event_requires_manual_review(true).await;
    }

    #[tokio::test]
    async fn codex_authentication_and_quota_errors_never_admit_protected_tools() {
        for (status, body) in [
            (401, json!({"error":{"code":"invalid_token"}}).to_string()),
            (
                429,
                json!({"error":{"code":"usage_limit_reached","message":"limit reached"}})
                    .to_string(),
            ),
        ] {
            let (requests, kinds, reason) = blocked_auto_review_case(vec![(status, body)]).await;
            assert_eq!(requests.len(), 3);
            assert_eq!(
                kinds
                    .iter()
                    .filter(|kind| **kind == RequestKind::PermissionCheck)
                    .count(),
                1
            );
            assert!(!reason.is_empty());
        }
    }
}
