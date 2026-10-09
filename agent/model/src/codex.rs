use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{
    CODEX_RESPONSES_URL, ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest,
    ModelRequestContext, ModelRequestStage, ModelResponseRejection, consume_sse_response,
    image_outside_user_message, is_context_limit_error, request_transport_error, responses,
    validate_reasoning_effort,
};
use crate::{
    ModelConfigExt,
    auth::{AuthError, CodexAuth},
    provider_failure::ProviderHttpFailureExt,
};
use tickloop_agent_base::network::{
    HttpPolicy, NetworkDiagnostic, NetworkRoute, build_http_client,
};
use tickloop_agent_protocol::{
    Adapter, Content, ModelAuthSource, ModelConfig, ModelIdentity, ModelProvider, Protocol,
    ProviderHttpFailure, ReasoningEffort, Role,
};

const CODEX_CLIENT_ORIGINATOR: &str = "tickloop";

#[cfg(test)]
mod error_tests;
mod errors;
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

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_loopback_test_auth(
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

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_loopback_test_endpoint(
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

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_loopback_test_websocket(mut self) -> Self {
        self.prefer_websocket = true;
        self
    }
}

impl CodexModelClient {
    async fn complete_inner(
        &self,
        chat_id: Option<tickloop_agent_protocol::ChatId>,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        model_context: Option<&ModelRequestContext>,
    ) -> Result<ModelOutput, ModelError> {
        let structured_output = request.structured_output.clone();
        if let Some(format) = &structured_output {
            format.validator()?;
        }
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
        match structured_output {
            Some(format) => format.validate_output(output),
            None => Ok(output),
        }
    }

    async fn complete_sse(
        &self,
        chat_id: Option<tickloop_agent_protocol::ChatId>,
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
            let (retry_after, server_retry) =
                super::provider_failure::retry_headers(response.headers());
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
            let mut failure = ProviderHttpFailure::from_response(
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
            Adapter::CodexResponses,
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

    async fn close_chat(&self, chat_id: tickloop_agent_protocol::ChatId) {
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
        chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(Some(chat_id), request, events, cancel, None)
            .await
    }

    async fn complete_for_chat_with_context(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
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
    if let Some(temperature) = super::sampling_temperature(
        config,
        request.temperature,
        super::ModelConfigExt::thinking_enabled(config, request.reasoning_effort, true)
            .unwrap_or(false),
    ) {
        body["temperature"] = json!(temperature);
    }
    if let Some(format) = request.structured_output {
        if !request.tools.is_empty() || format.name.trim().is_empty() {
            return Err(ModelError::Configuration(
                "Codex structured output requires a named JSON schema without executable tools"
                    .into(),
            ));
        }
        let instructions = body["instructions"].as_str().unwrap().to_owned();
        body["instructions"] = json!(format!("{instructions}\n\n{}", format.prompt()));
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
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
    use crate::{cards::ModelCatalog, codex_test_support::*};
    use tickloop_agent_protocol::Message;
    use tickloop_agent_protocol::Usage;

    #[test]
    fn structured_request_uses_prompted_json_without_result_tools() {
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
                messages: vec![Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Review".into(),
                    }],
                }],
                tools: Vec::new(),
                structured_output: Some(crate::StructuredOutput {
                    name: "classify_result".into(),
                    schema: schema.clone(),
                }),
                temperature: Some(0.0),
                reasoning_effort: None,
            },
            "test-origin",
        )
        .unwrap();
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
        assert!(body.get("tools").is_none());
        assert!(
            body["instructions"]
                .as_str()
                .unwrap()
                .contains(&schema.to_string())
        );
        assert!(body["text"]["format"].is_null());
    }

    #[tokio::test]
    async fn structured_sse_accepts_non_object_schemas_and_still_validates_results() {
        for (schema, result, valid) in [
            (
                json!({"type":"array","items":{"type":"integer"}}),
                json!([1, 2]),
                true,
            ),
            (
                json!({"anyOf":[{"type":"boolean"},{"type":"null"}]}),
                json!(true),
                true,
            ),
            (
                json!({"type":"array","items":{"type":"integer"}}),
                json!({}),
                false,
            ),
        ] {
            let (_directory, state_dir) = state_with_credentials();
            let message = json!({"type":"message","id":"json-result","role":"assistant","status":"completed","content":[{"type":"output_text","text":result.to_string()}]});
            let response = [
                event(
                    "response.created",
                    0,
                    json!({"response":{"id":"json","status":"in_progress"}}),
                ),
                event(
                    "response.output_item.added",
                    1,
                    json!({"output_index":0,"item":{"type":"message","id":"json-result","role":"assistant","status":"in_progress","content":[]}}),
                ),
                event(
                    "response.output_item.done",
                    2,
                    json!({"output_index":0,"item":message}),
                ),
                event(
                    "response.done",
                    3,
                    json!({"response":{"id":"json","status":"completed","output":[message]}}),
                ),
            ]
            .concat();
            let (endpoint, captured) = serve_scripted_responses(vec![(200, response)]).await;
            let config = ModelCatalog::defaults()
                .get("openai-codex", "gpt-5.5")
                .unwrap();
            let client = CodexModelClient::new(config, &state_dir)
                .unwrap()
                .with_loopback_test_endpoint(endpoint.as_str())
                .unwrap();
            let mut request = simple_request();
            request.structured_output = Some(crate::StructuredOutput {
                name: "json_result".into(),
                schema,
            });
            let (events, _) = tokio::sync::mpsc::unbounded_channel();
            let output = client
                .complete(request, events, CancellationToken::new())
                .await;
            if valid {
                assert_eq!(
                    output.unwrap().content,
                    vec![Content::Text {
                        text: result.to_string()
                    }]
                );
            } else {
                assert!(matches!(
                    output,
                    Err(ModelError::ResponseRejected(
                        ModelResponseRejection::MalformedStructuredOutput
                    ))
                ));
            }
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].get("tools").is_none());
        }
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
                    Message {
                        role: Role::Assistant,
                        content: vec![Content::ToolUse {
                            id: wire_id.clone(),
                            name: "Read".into(),
                            input: json!({"file_path":"notes.txt"}),
                        }],
                    },
                    Message {
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
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        };
        tag_private_output(&mut output, "account-a");
        let mut request = simple_request();
        request.messages = vec![Message {
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
        let mut messages = vec![Message {
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
            messages.push(Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: id.clone(),
                    name: "Read".into(),
                    input: json!({"file_path":"note.txt"}),
                }],
            });
            messages.push(Message {
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

    fn simple_request() -> ModelRequest {
        ModelRequest {
            history_sources: Vec::new(),
            messages: vec![Message {
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
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Hello".into(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: first.content,
                },
                Message {
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
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Hello".into(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: first.content,
                },
                Message {
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
            event("response.reasoning_summary_text.delta", 2, json!({"output_index":0,"item_id":"rs-partial","summary_index":0,"delta":"Thinking"})),
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
    async fn codex_raw_reasoning_prefix_completion_delivers_normal_answers_and_rejects_conflicts() {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| profile.provider == ModelProvider::OpenAiCodex)
            .unwrap();
        for added_prefix in [true, false] {
            for completed in ["Thinking", "Contradictory"] {
                let (endpoint, captured) = serve_scripted_responses(vec![(
                    200,
                    tickloop_agent_test_support::model_http_support::raw_reasoning_prefix_reply(
                        added_prefix,
                        completed,
                        false,
                    ),
                )])
                .await;
                let client = CodexModelClient::new(config.clone(), &state_dir)
                    .unwrap()
                    .with_loopback_test_endpoint(endpoint.as_str())
                    .unwrap();
                let (events, mut deltas) = tokio::sync::mpsc::unbounded_channel();
                let result = client
                    .complete(simple_request(), events, CancellationToken::new())
                    .await;
                captured.await.unwrap();
                if completed == "Contradictory" {
                    assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
                    continue;
                }
                let output = result.expect("Codex reasoning prefixes must allow a normal answer");
                let mut thinking = String::new();
                let mut text = String::new();
                while let Ok(delta) = deltas.try_recv() {
                    match delta {
                        ModelDelta::Thinking(value) => thinking.push_str(&value),
                        ModelDelta::Text(value) => text.push_str(&value),
                        _ => {}
                    }
                }
                assert_eq!(thinking, "Thinking");
                assert_eq!(text, "Answer");
                assert!(
                    matches!(&output.content[0], Content::ProtocolData { value, .. } if value["content"][0]["text"] == "Thinking" && value["encrypted_content"] == "opaque-fixture")
                );
            }
        }
    }

    #[tokio::test]
    async fn prompt_suggestion_codex_completion_only_reasoning_hits_budget_before_terminal_event() {
        for phase in ["added", "done"] {
            let (_directory, state_dir) = state_with_credentials();
            let config = ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .find(|profile| profile.provider == ModelProvider::OpenAiCodex)
                .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
            let (started, received) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                let reasoning = json!({"type":"reasoning","id":"rs-completion-only","summary":[{"type":"summary_text","text":"x".repeat(2_048)}],"encrypted_content":"opaque-fixture"});
                let mut initial = reasoning.clone();
                if phase == "done" {
                    initial["summary"] = json!([]);
                }
                let mut stream = event(
                    "response.created",
                    0,
                    json!({"response":{"id":"completion-only","status":"in_progress"}}),
                );
                stream.push_str(&event(
                    "response.output_item.added",
                    1,
                    json!({"output_index":0,"item":initial}),
                ));
                if phase == "done" {
                    stream.push_str(&event(
                        "response.output_item.done",
                        2,
                        json!({"output_index":0,"item":reasoning}),
                    ));
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                socket.write_all(stream.as_bytes()).await.unwrap();
                started.send(()).unwrap();
                let mut byte = [0];
                let closed =
                    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await;
                assert!(
                    matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
                    "budget must close completion-only transport"
                );
            });
            let budget = super::super::GenerationBudget::new(1_024, Duration::from_secs(10));
            let client = CodexModelClient::new(config, &state_dir)
                .unwrap()
                .with_loopback_test_endpoint(&endpoint)
                .unwrap();
            let cancel = CancellationToken::new();
            let request_cancel = cancel.clone();
            let generation = tokio::spawn(async move {
                budget
                    .complete(
                        &client,
                        uuid::Uuid::new_v4(),
                        simple_request(),
                        request_cancel,
                    )
                    .await
            });
            received.await.unwrap();
            let result = tokio::time::timeout(Duration::from_millis(500), generation).await;
            assert!(
                result.is_ok(),
                "{phase}: completion-only reasoning bypassed budget"
            );
            assert!(matches!(
                result.unwrap().unwrap(),
                Err(ModelError::ResponseRejected(
                    ModelResponseRejection::OutputLimit
                ))
            ));
            assert!(cancel.is_cancelled());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn prompt_suggestion_codex_budget_cuts_text_reasoning_and_deadline_transports() {
        for kind in ["text", "thinking", "deadline"] {
            let (_directory, state_dir) = state_with_credentials();
            let config = ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .find(|profile| profile.provider == ModelProvider::OpenAiCodex)
                .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
            let (started, received) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (_, body) = read_request(&mut socket).await;
                assert!(body.get("max_output_tokens").is_none());
                assert!(body.get("tools").is_none());
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                let mut stream = event(
                    "response.created",
                    0,
                    json!({"response":{"id":"bounded","status":"in_progress"}}),
                );
                if kind == "text" {
                    stream.push_str(&event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"message","id":"msg-bounded","role":"assistant","content":[]}})));
                    stream.push_str(&event("response.content_part.added", 2, json!({"output_index":0,"item_id":"msg-bounded","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
                    stream.push_str(&event("response.output_text.delta", 3, json!({"output_index":0,"item_id":"msg-bounded","content_index":0,"delta":"x".repeat(1_024)})));
                } else if kind == "thinking" {
                    stream.push_str(&event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"reasoning","id":"rs-bounded","summary":[]}})));
                    stream.push_str(&event(
                        "response.reasoning_summary_text.delta",
                        2,
                        json!({"output_index":0,"item_id":"rs-bounded","summary_index":0,"delta":"x".repeat(1_024)}),
                    ));
                }
                socket.write_all(stream.as_bytes()).await.unwrap();
                started.send(()).unwrap();
                let mut byte = [0];
                let closed =
                    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await;
                assert!(
                    matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
                    "budget must close Codex transport"
                );
            });
            let deadline = if kind == "deadline" {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(10)
            };
            let budget = super::super::GenerationBudget::new(1_024, deadline);
            let client = CodexModelClient::new(budget.apply_model_limit(config), &state_dir)
                .unwrap()
                .with_loopback_test_endpoint(&endpoint)
                .unwrap();
            let cancel = CancellationToken::new();
            let request_cancel = cancel.clone();
            let generation = tokio::spawn(async move {
                budget
                    .complete(
                        &client,
                        uuid::Uuid::new_v4(),
                        simple_request(),
                        request_cancel,
                    )
                    .await
            });
            received.await.unwrap();
            let result = tokio::time::timeout(Duration::from_millis(500), generation)
                .await
                .unwrap()
                .unwrap();
            if kind == "deadline" {
                assert!(matches!(result, Err(ModelError::TimedOut)));
            } else {
                assert!(matches!(
                    result,
                    Err(ModelError::ResponseRejected(
                        ModelResponseRejection::OutputLimit
                    ))
                ));
            }
            assert!(cancel.is_cancelled());
            server.await.unwrap();
        }
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
                    crate::StructuredOutput {
                        name: "classify_result".into(),
                        schema: json!({"type":"object"})
                    }
                    .validate_output(output),
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
            request.structured_output = Some(crate::StructuredOutput {
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
}
