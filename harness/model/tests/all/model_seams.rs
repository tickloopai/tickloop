use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use serde_json::{Value, json};
use tickloop_harness_model::{
    AttemptGate, Credential, CredentialSource, HttpModelClients, LocalCredentials,
    LocalModelClients, ModelClient, ModelClients, ModelConfigExt, ModelError, ModelOutput,
    ModelRequest, ModelRequestContext, ModelRequestStage, ProxyEndpoint, RejectionOutcome,
    ResolvedCredential, VendorEndpoint, cards::ModelCatalog, credentials::save_api_key,
};
use tickloop_harness_protocol::{
    Content, ImageMediaType, ImageSource, InputModality, Message, ModelAuthSource, ModelConfig,
    ModelProvider, PrivateHistorySource, Protocol, Role,
};
use tickloop_harness_test_support::model_http_support::{
    serve_responses, serve_router_with_status,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// SHA-256 of `fixture-api-key`, the identity local API-key history has always recorded.
const FIXTURE_KEY_IDENTITY: &str =
    "fdab98033749d02f02a57999bbe556414868bc3846375756506317674bace604";
const PLATFORM_IDENTITY: &str = "fixture-provider:platform-key";

fn messages_config(base_url: String, api_key_env: &str) -> ModelConfig {
    ModelConfig {
        name: "fixture".into(),
        protocol: Protocol::Messages,
        base_url,
        model: "fixture-model".into(),
        api_key_env: api_key_env.into(),
        max_output_tokens: 32,
        provider: ModelProvider::Api,
        auth_source: ModelAuthSource::ApiKeyEnv,
        provider_id: "fixture-provider".into(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text, InputModality::Image],
    }
}

fn messages_stream(blocks: Vec<Value>) -> String {
    let frame = |kind: &str, mut value: Value| {
        value["type"] = json!(kind);
        format!("event: {kind}\ndata: {value}\n\n")
    };
    let mut output = frame(
        "message_start",
        json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":1}}}),
    );
    for (index, block) in blocks.into_iter().enumerate() {
        output += &frame(
            "content_block_start",
            json!({"index":index,"content_block":block}),
        );
        output += &frame("content_block_stop", json!({"index":index}));
    }
    output += &frame(
        "message_delta",
        json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
    );
    output + &frame("message_stop", json!({}))
}

fn private_answer() -> String {
    messages_stream(vec![
        json!({"type":"thinking","thinking":"private reasoning","signature":"signature"}),
        json!({"type":"text","text":"answer"}),
    ])
}

fn text_answer() -> String {
    messages_stream(vec![json!({"type":"text","text":"answer"})])
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn request(
    messages: Vec<Message>,
    history_sources: Vec<Option<PrivateHistorySource>>,
) -> ModelRequest {
    ModelRequest {
        deferred_tools: Vec::new(),
        history_sources,
        messages,
        tools: Vec::new(),
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

fn image_request() -> ModelRequest {
    request(
        vec![Message {
            role: Role::User,
            content: vec![
                Content::Text {
                    text: "Describe it".into(),
                },
                Content::Image {
                    source: ImageSource::Base64 {
                        media_type: ImageMediaType::Png,
                        data: "image-data".into(),
                    },
                    width: 16,
                    height: 16,
                },
            ],
        }],
        Vec::new(),
    )
}

/// A follow-up request that replays the private blocks of `first`.
fn follow_up(first: &ModelOutput, with_sources: bool) -> ModelRequest {
    let messages = vec![
        user("Hello"),
        Message {
            role: Role::Assistant,
            content: first.content.clone(),
        },
        user("Continue"),
    ];
    let sources = if with_sources {
        vec![None, first.private_source.clone(), None]
    } else {
        Vec::new()
    };
    request(messages, sources)
}

async fn complete(
    client: &dyn ModelClient,
    chat: uuid::Uuid,
    request: ModelRequest,
) -> Result<ModelOutput, ModelError> {
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete_for_chat(chat.into(), request, events, CancellationToken::new())
        .await
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn sent_block_types(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap().clone())
        .map(|block| block["type"].as_str().unwrap().to_owned())
        .collect()
}

/// A platform credential whose secret rotates on every refresh while its identity stays.
#[derive(Default)]
struct PlatformCredentials {
    refreshes: AtomicUsize,
    rotate_on_resolve: bool,
    current: Mutex<Option<String>>,
}

impl PlatformCredentials {
    fn refresh(&self, gate: AttemptGate<'_>) -> Result<(), ModelError> {
        if !gate.try_begin_attempt() {
            return Err(ModelError::RequestBudgetExhausted {
                stage: ModelRequestStage::Credentials,
            });
        }
        let generation = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        *self.current.lock().unwrap() = Some(format!("token-{generation}"));
        Ok(())
    }

    fn current(&self) -> ResolvedCredential {
        let token = self.current.lock().unwrap().clone().unwrap();
        ResolvedCredential::new(
            HeaderValue::from_str(&token).unwrap(),
            PLATFORM_IDENTITY.into(),
        )
    }
}

#[async_trait]
impl CredentialSource for PlatformCredentials {
    async fn resolve(
        &self,
        _config: &ModelConfig,
        gate: AttemptGate<'_>,
        _cancel: &CancellationToken,
    ) -> Result<Credential, ModelError> {
        if self.rotate_on_resolve || self.current.lock().unwrap().is_none() {
            self.refresh(gate)?;
        }
        Ok(Credential::Resolved(self.current()))
    }

    async fn rejected(
        &self,
        _config: &ModelConfig,
        _credential: &ResolvedCredential,
        gate: AttemptGate<'_>,
        _cancel: &CancellationToken,
    ) -> Result<RejectionOutcome, ModelError> {
        self.refresh(gate)?;
        Ok(RejectionOutcome::Refreshed(self.current()))
    }
}

#[tokio::test]
async fn local_api_key_history_source_keeps_its_persisted_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_api_key(&root, "FIXTURE_SEAM_KEY", "fixture-api-key").unwrap();
    let (base_url, mut requests) = serve_responses(vec![(200, private_answer())]).await;
    let config = messages_config(base_url, "FIXTURE_SEAM_KEY");
    let client = LocalModelClients::new(Some(root)).client(&config).unwrap();

    let output = complete(
        &*client,
        uuid::Uuid::new_v4(),
        request(vec![user("Hello")], Vec::new()),
    )
    .await
    .unwrap();

    let mut expected = config.private_history_source().unwrap();
    expected.credential_identity = Some(FIXTURE_KEY_IDENTITY.into());
    assert_eq!(
        serde_json::to_vec(&output.private_source).unwrap(),
        serde_json::to_vec(&Some(expected)).unwrap()
    );
    let (headers, _) = requests.recv().await.unwrap();
    assert_eq!(header(&headers, "x-api-key"), Some("fixture-api-key"));
}

#[tokio::test]
async fn anonymous_loopback_request_carries_no_credential() {
    let (base_url, mut requests) = serve_responses(vec![(200, private_answer())]).await;
    let config = messages_config(base_url, "");
    let credential = LocalCredentials::new(None)
        .resolve(&config, AttemptGate::new(None), &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(credential, Credential::Anonymous));

    let client = LocalModelClients::new(None).client(&config).unwrap();
    let output = complete(
        &*client,
        uuid::Uuid::new_v4(),
        request(vec![user("Hello")], Vec::new()),
    )
    .await
    .unwrap();

    assert_eq!(output.private_source.unwrap().credential_identity, None);
    let (headers, _) = requests.recv().await.unwrap();
    assert_eq!(header(&headers, "x-api-key"), None);
    assert_eq!(header(&headers, "authorization"), None);
}

#[tokio::test]
async fn proxy_endpoint_keeps_vendor_paths_of_built_in_selections() {
    let (server_url, mut requests) = serve_router_with_status(|_| (400, "{}".into())).await;
    let proxy_base = server_url.replace("/anthropic", "/model-proxy");
    let clients = HttpModelClients::new(
        Arc::new(ProxyEndpoint::new(reqwest::Url::parse(&proxy_base).unwrap()).unwrap()),
        Arc::new(PlatformCredentials::default()),
    );
    let catalog = ModelCatalog::defaults();
    let selections = [
        (
            "deepseek-api",
            "deepseek-flash",
            "/model-proxy/deepseek-api/chat/completions",
        ),
        (
            "minimax-cn-api",
            "MiniMax-M3",
            "/model-proxy/minimax-cn-api/anthropic/v1/messages",
        ),
        (
            "minimax-global-api",
            "MiniMax-M3",
            "/model-proxy/minimax-global-api/anthropic/v1/messages",
        ),
    ];
    for (provider, model, path) in selections {
        let config = catalog.get(provider, model).unwrap();
        let vendor_base_url = config.base_url.clone();
        let client = clients.client(&config).unwrap();

        let error = complete(
            &*client,
            uuid::Uuid::new_v4(),
            request(vec![user("Hello")], Vec::new()),
        )
        .await
        .unwrap_err();

        assert_eq!(error.status(), Some(400), "{provider}/{model}");
        let (headers, _) = requests.recv().await.unwrap();
        assert_eq!(
            headers.lines().next().unwrap(),
            format!("POST {path} HTTP/1.1"),
            "{provider}/{model}"
        );
        assert_eq!(header(&headers, "authorization"), Some("Bearer token-1"));
        assert_eq!(header(&headers, "x-api-key"), None);
        assert_eq!(config.base_url, vendor_base_url);
    }
}

#[tokio::test]
async fn rotated_platform_token_keeps_private_history_compatible() {
    let (base_url, mut requests) = serve_router_with_status(|_| (200, private_answer())).await;
    let config = messages_config(base_url, "FIXTURE_PLATFORM_KEY");
    let clients = HttpModelClients::new(
        Arc::new(VendorEndpoint),
        Arc::new(PlatformCredentials {
            rotate_on_resolve: true,
            ..Default::default()
        }),
    );
    let client = clients.client(&config).unwrap();
    let chat = uuid::Uuid::new_v4();

    let first = complete(&*client, chat, request(vec![user("Hello")], Vec::new()))
        .await
        .unwrap();
    assert_eq!(
        first
            .private_source
            .as_ref()
            .unwrap()
            .credential_identity
            .as_deref(),
        Some(PLATFORM_IDENTITY)
    );
    complete(&*client, chat, follow_up(&first, true))
        .await
        .unwrap();
    complete(&*client, chat, follow_up(&first, false))
        .await
        .unwrap();

    let tokens: Vec<_> = (0..3)
        .map(|_| requests.try_recv().unwrap())
        .map(|(headers, body)| {
            (
                header(&headers, "x-api-key").unwrap().to_owned(),
                sent_block_types(&body),
            )
        })
        .collect();
    assert_eq!(tokens[0].0, "token-1");
    assert_eq!(tokens[1].0, "token-2");
    assert_eq!(tokens[2].0, "token-3");
    for (_, blocks) in &tokens[1..] {
        assert!(blocks.contains(&"thinking".to_owned()), "{blocks:?}");
    }
}

#[tokio::test]
async fn model_clients_instances_keep_their_own_private_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_api_key(&root, "FIXTURE_SEAM_KEY", "fixture-api-key").unwrap();
    let (base_url, _requests) = serve_router_with_status(|_| (200, private_answer())).await;
    let config = messages_config(base_url, "FIXTURE_SEAM_KEY");
    let first = LocalModelClients::new(Some(root.clone()))
        .client(&config)
        .unwrap();
    let second = LocalModelClients::new(Some(root)).client(&config).unwrap();
    let chat = uuid::Uuid::new_v4();

    let first_output = complete(&*first, chat, request(vec![user("Hello")], Vec::new()))
        .await
        .unwrap();
    let second_output = complete(&*second, chat, request(vec![user("Hello")], Vec::new()))
        .await
        .unwrap();
    second.close_chat(chat.into()).await;

    complete(&*first, chat, follow_up(&first_output, false))
        .await
        .unwrap();
    let error = complete(&*second, chat, follow_up(&second_output, false))
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Configuration(_)), "{error:?}");
}

const IMAGE_REJECTION: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"image exceeds 5 MB maximum"}}"#;

/// Runs one runtime-style attempt (the runtime begins an attempt before each model call)
/// through a credential refresh, a 401 refresh and an image fallback.
async fn budgeted_attempt(
    max_attempts: usize,
    responses: Vec<(u16, String)>,
) -> (
    Result<ModelOutput, ModelError>,
    ModelRequestContext,
    Vec<(String, Value)>,
) {
    let (base_url, mut requests) = serve_responses(responses).await;
    let clients = HttpModelClients::new(
        Arc::new(VendorEndpoint),
        Arc::new(PlatformCredentials::default()),
    );
    let client = clients
        .client(&messages_config(base_url, "FIXTURE_PLATFORM_KEY"))
        .unwrap();
    let context = ModelRequestContext::new(max_attempts);
    assert!(context.try_begin_attempt());
    let (events, _) = mpsc::unbounded_channel();
    let result = client
        .complete_for_chat_with_context(
            uuid::Uuid::new_v4().into(),
            image_request(),
            events,
            CancellationToken::new(),
            context.clone(),
        )
        .await;
    let mut sent = Vec::new();
    while let Ok(request) = requests.try_recv() {
        sent.push(request);
    }
    (result, context, sent)
}

#[tokio::test]
async fn refresh_unauthorized_retry_and_image_fallback_share_one_attempt_budget() {
    let responses = || {
        vec![
            (401, "{}".to_owned()),
            (400, IMAGE_REJECTION.to_owned()),
            (200, text_answer()),
        ]
    };

    let (result, context, sent) = budgeted_attempt(5, responses()).await;
    result.unwrap();
    assert_eq!(context.used_attempts(), 5);
    let keys: Vec<_> = sent
        .iter()
        .map(|(headers, _)| header(headers, "x-api-key").unwrap().to_owned())
        .collect();
    assert_eq!(keys, ["token-1", "token-2", "token-2"]);
    assert!(sent_block_types(&sent[1].1).contains(&"image".to_owned()));
    assert!(!sent_block_types(&sent[2].1).contains(&"image".to_owned()));

    let (result, context, sent) = budgeted_attempt(4, responses()).await;
    assert!(matches!(
        result,
        Err(ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::RequestSend
        })
    ));
    assert_eq!(context.used_attempts(), 4);
    assert_eq!(sent.len(), 2);

    let (result, context, sent) = budgeted_attempt(3, responses()).await;
    assert!(matches!(
        result,
        Err(ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::RequestSend
        })
    ));
    assert_eq!(context.used_attempts(), 3);
    assert_eq!(sent.len(), 1);

    let (result, context, sent) = budgeted_attempt(2, responses()).await;
    assert!(matches!(
        result,
        Err(ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::Credentials
        })
    ));
    assert_eq!(context.used_attempts(), 2);
    assert_eq!(sent.len(), 1);

    let (result, context, sent) = budgeted_attempt(1, responses()).await;
    assert!(matches!(
        result,
        Err(ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::Credentials
        })
    ));
    assert_eq!(context.used_attempts(), 1);
    assert!(sent.is_empty());
}

#[tokio::test]
async fn unauthorized_request_is_resent_once_with_the_refreshed_credential() {
    let (base_url, mut requests) = serve_responses(vec![
        (401, "{}".to_owned()),
        (401, "{}".to_owned()),
        (200, text_answer()),
    ])
    .await;
    let credentials = Arc::new(PlatformCredentials::default());
    let client = HttpModelClients::new(Arc::new(VendorEndpoint), credentials.clone())
        .client(&messages_config(base_url, "FIXTURE_PLATFORM_KEY"))
        .unwrap();

    let error = complete(
        &*client,
        uuid::Uuid::new_v4(),
        request(vec![user("Hello")], Vec::new()),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status(), Some(401));
    assert_eq!(credentials.refreshes.load(Ordering::SeqCst), 2);
    let keys: Vec<_> = (0..2)
        .map(|_| requests.try_recv().unwrap())
        .map(|(headers, _)| header(&headers, "x-api-key").unwrap().to_owned())
        .collect();
    assert_eq!(keys, ["token-1", "token-2"]);
    assert!(requests.try_recv().is_err());
}
