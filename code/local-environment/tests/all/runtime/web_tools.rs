use crate::runtime::support;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    ApprovalRecord, ApprovalResult, ChatSnapshot, Content, Decision, EventKind, InputModality,
    ModelConfig, ModelRequestKind, PermissionRequest, PermissionRule, PermissionScope, Protocol,
    RequestFailure, RequestOutcome, Role, SearchConfig, ToolResult, Usage,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::tools::permissions::permission_rule_candidates;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Notify, mpsc::UnboundedSender},
};
use tokio_util::sync::CancellationToken;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);
const PAGE_PROMPT_START: &str = "\nWeb page content:\n---\n";
const SECONDARY_GUIDELINES: &str = "Provide a concise response based only on the content above. In your response:\n - Enforce a strict 125-character maximum for quotes from any source document.";
const SOURCES_REMINDER: &str = "\n\n\nREMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.";

/// How the Chat model answers the separate request `WebFetch` makes about a page.
enum PageAnswer {
    Text(&'static str),
    Failure,
    /// Fails with a transient service error once, then answers.
    UnavailableOnce(&'static str),
    Pending(Arc<Notify>),
}

/// Makes the scripted tool calls in order, then ends the reply; requests without tools are
/// `WebFetch` asking about a page.
struct WebModel {
    calls: Vec<(&'static str, Value)>,
    page_answer: PageAnswer,
    page_requests: Mutex<Vec<ModelRequest>>,
    conversation_requests: Mutex<Vec<ModelRequest>>,
}

impl WebModel {
    fn new(calls: Vec<(&'static str, Value)>, page_answer: PageAnswer) -> Arc<Self> {
        Arc::new(Self {
            calls,
            page_answer,
            page_requests: Mutex::new(Vec::new()),
            conversation_requests: Mutex::new(Vec::new()),
        })
    }

    fn page_prompts(&self) -> Vec<String> {
        self.page_requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                assert!(request.tools.is_empty());
                assert_eq!(request.messages.len(), 1);
                assert_eq!(request.messages[0].role, Role::User);
                let [Content::Text { text }] = request.messages[0].content.as_slice() else {
                    panic!("the page request is one text prompt");
                };
                text.clone()
            })
            .collect()
    }
}

#[async_trait]
impl ModelClient for WebModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
        tickloop_harness::protocol::ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_harness::protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if request.tools.is_empty() {
            let attempt = {
                let mut page_requests = self.page_requests.lock().unwrap();
                page_requests.push(request);
                page_requests.len()
            };
            return match &self.page_answer {
                PageAnswer::Text(text) => Ok(text_output(text)),
                PageAnswer::Failure => Err(ModelError::Authentication),
                PageAnswer::UnavailableOnce(_) if attempt == 1 => {
                    Err(ModelError::Http { status: 503 })
                }
                PageAnswer::UnavailableOnce(text) => Ok(text_output(text)),
                PageAnswer::Pending(entered) => {
                    entered.notify_one();
                    std::future::pending().await
                }
            };
        }
        let completed_calls = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|content| matches!(content, Content::ToolResult { .. }))
            .count();
        self.conversation_requests.lock().unwrap().push(request);
        Ok(match self.calls.get(completed_calls) {
            Some((name, input)) => ModelOutput {
                private_source: None,
                content: vec![Content::ToolUse {
                    id: format!("web-call-{completed_calls}"),
                    name: (*name).into(),
                    input: input.clone(),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            },
            None => text_output("The external sources have been read."),
        })
    }
}

fn text_output(text: &str) -> ModelOutput {
    ModelOutput {
        private_source: None,
        content: vec![Content::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: "end_turn".into(),
    }
}

fn config() -> ModelConfig {
    ModelConfig {
        provider: tickloop_harness::protocol::ModelProvider::Api,
        auth_source: tickloop_harness::protocol::ModelAuthSource::ApiKeyEnv,
        name: "test".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "test".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    }
}

fn response(status: u16, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} Fixture\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves `responses` in order, repeating the last one, and counts the connections it accepts.
async fn server(responses: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let index = accepted.fetch_add(1, Ordering::SeqCst);
            let reply = responses[index.min(responses.len() - 1)].clone();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0; 1024];
                let length = socket.read(&mut bytes).await.unwrap();
                if length == 0 {
                    break;
                }
                request.extend_from_slice(&bytes[..length]);
            }
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}"), connections)
}

struct WebRun {
    _directory: tempfile::TempDir,
    permission_requests: Vec<(String, Value)>,
    results: Vec<ToolResult>,
    retried_requests: Vec<ModelRequestKind>,
    failure: Option<String>,
    snapshot: ChatSnapshot,
    approvals: Vec<ApprovalRecord>,
}

/// How the user answers a permission request.
enum Reply {
    Allow(PermissionScope),
    /// "Yes, and don't ask again" with the first rule the approval surface offers.
    AllowWithSuggestedRule,
    Deny,
}

async fn run_web(model: Arc<WebModel>, search: Option<SearchConfig>) -> WebRun {
    run_web_with(model, search, Vec::new(), |_| {
        Reply::Allow(PermissionScope::Once)
    })
    .await
}

/// Runs one request in a new Chat holding `rules`, answering each permission request by `reply`.
async fn run_web_with(
    model: Arc<WebModel>,
    search: Option<SearchConfig>,
    rules: Vec<PermissionRule>,
    reply: impl Fn(&PermissionRequest) -> Reply,
) -> WebRun {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_search(chat, search).await.unwrap();
    agent.set_permission_rules(chat, rules).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read the requested external information".into())
        .await
        .unwrap();
    let mut permission_requests = Vec::new();
    let mut results = Vec::new();
    let mut retried_requests = Vec::new();
    let failure = loop {
        match tokio::time::timeout(EVENT_TIMEOUT, events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => {
                permission_requests.push((request.name.clone(), request.input.clone()));
                match reply(&request) {
                    Reply::Allow(scope) => agent
                        .answer_permission_with_scope(chat, request.id, true, scope)
                        .await
                        .unwrap(),
                    Reply::AllowWithSuggestedRule => {
                        let suggestion = permission_rule_candidates(&request.name, &request.input)
                            .into_iter()
                            .next()
                            .expect("the approval surface suggests a rule");
                        agent
                            .answer_permission_with_rule(chat, request.id, suggestion.specifier)
                            .await
                            .unwrap();
                    }
                    Reply::Deny => agent
                        .answer_permission(chat, request.id, false)
                        .await
                        .unwrap(),
                }
            }
            EventKind::ToolCompleted { result, .. } => results.push(result),
            EventKind::ModelRetry { kind, .. } => retried_requests.push(kind),
            EventKind::Completed { .. } | EventKind::Interrupted => break None,
            EventKind::Failed(error) => break Some(error),
            _ => {}
        }
    };
    let snapshot = agent.snapshot(chat).await.unwrap();
    let approvals = agent.approval_history_view(chat).await.unwrap().records;
    agent.shutdown().await;
    WebRun {
        _directory: directory,
        permission_requests,
        results,
        retried_requests,
        failure,
        snapshot,
        approvals,
    }
}

fn fetch_call(url: &str, prompt: &str) -> (&'static str, Value) {
    ("WebFetch", json!({"url": url, "prompt": prompt}))
}

fn conversation_mentions(snapshot: &ChatSnapshot, text: &str) -> bool {
    serde_json::to_string(&snapshot.messages)
        .unwrap()
        .contains(text)
}

fn request_kinds(snapshot: &ChatSnapshot) -> Vec<(ModelRequestKind, RequestOutcome)> {
    support::model_script_requests(snapshot)
        .iter()
        .map(|entry| (entry.kind, entry.outcome))
        .collect()
}

#[tokio::test]
async fn fetch_answers_the_prompt_about_the_page_markdown_in_a_request_outside_the_conversation() {
    let (url, _) = server(vec![
        response(302, "Location: /article\r\n", ""),
        response(
            200,
            "Content-Type: text/html; charset=utf-8\r\n",
            "<html><head><style>hidden style</style></head><body><h1>Delivery update</h1><p>Delivery is Friday &amp; confirmed.</p><script>hidden script</script></body></html>",
        ),
    ])
    .await;
    let model = WebModel::new(
        vec![fetch_call(&format!("{url}/start"), "When is delivery?")],
        PageAnswer::Text("Delivery is on Friday."),
    );
    let run = run_web(model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(
        run.permission_requests,
        vec![(
            "WebFetch".to_owned(),
            json!({"url": format!("{url}/start"), "prompt": "When is delivery?"})
        )]
    );
    assert_eq!(run.results.len(), 1);
    assert!(!run.results[0].is_error, "{}", run.results[0].content);
    assert_eq!(run.results[0].content, "Delivery is on Friday.");

    let prompts = model.page_prompts();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].starts_with(&format!(
            "{PAGE_PROMPT_START}# Delivery update\n\nDelivery is Friday & confirmed.\n---\n\nWhen is delivery?\n\n{SECONDARY_GUIDELINES}"
        )),
        "{}",
        prompts[0]
    );
    assert!(!prompts[0].contains("hidden"));

    let conversation = model.conversation_requests.lock().unwrap();
    let last_request = serde_json::to_string(&conversation.last().unwrap().messages).unwrap();
    assert!(last_request.contains("Delivery is on Friday."));
    assert!(!last_request.contains("Web page content"));
    drop(conversation);
    assert!(!conversation_mentions(&run.snapshot, "Web page content"));
    assert_eq!(
        request_kinds(&run.snapshot),
        vec![
            (ModelRequestKind::Conversation, RequestOutcome::Accepted),
            (ModelRequestKind::WebFetch, RequestOutcome::Accepted),
            (ModelRequestKind::Conversation, RequestOutcome::Accepted),
        ]
    );
    let entries = support::model_script_requests(&run.snapshot);
    assert_eq!(entries[1].run_id, entries[0].run_id);
    assert_eq!(entries[1].loop_id, entries[0].loop_id);
}

#[tokio::test]
async fn a_cached_page_is_asked_the_new_prompt_without_a_second_download() {
    let (url, connections) = server(vec![response(
        200,
        "Content-Type: text/plain\r\n",
        "Delivery is Friday.",
    )])
    .await;
    let model = WebModel::new(
        vec![
            fetch_call(&url, "When is delivery?"),
            fetch_call(&url, "Is delivery confirmed?"),
        ],
        PageAnswer::Text("Friday."),
    );
    let run = run_web(model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(run.results.len(), 2);
    assert!(run.results.iter().all(|result| !result.is_error));
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    let prompts = model.page_prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].starts_with(&format!(
        "{PAGE_PROMPT_START}Delivery is Friday.\n---\n\nIs delivery confirmed?\n\n"
    )));
}

#[tokio::test]
async fn a_failed_page_request_is_reported_as_a_clear_tool_error() {
    let (url, _) = server(vec![response(
        200,
        "Content-Type: text/plain\r\n",
        "Delivery is Friday.",
    )])
    .await;
    let model = WebModel::new(
        vec![fetch_call(&url, "When is delivery?")],
        PageAnswer::Failure,
    );
    let run = run_web(model, None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(run.results.len(), 1);
    assert!(run.results[0].is_error);
    assert_eq!(
        run.results[0].content,
        "the model request that applies the prompt to the fetched content failed: model authentication failed; review this connection in /provider → Connection Details"
    );
    assert!(request_kinds(&run.snapshot).contains(&(
        ModelRequestKind::WebFetch,
        RequestOutcome::Failed(RequestFailure::Authentication)
    )));
}

#[tokio::test]
async fn a_transient_page_request_failure_is_retried_like_other_model_requests() {
    let (url, _) = server(vec![response(
        200,
        "Content-Type: text/plain\r\n",
        "Delivery is Friday.",
    )])
    .await;
    let model = WebModel::new(
        vec![fetch_call(&url, "When is delivery?")],
        PageAnswer::UnavailableOnce("Friday."),
    );
    let run = run_web(model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(run.results[0].content, "Friday.");
    assert_eq!(run.retried_requests, vec![ModelRequestKind::WebFetch]);
    assert_eq!(model.page_prompts().len(), 2);
    assert_eq!(
        request_kinds(&run.snapshot),
        vec![
            (ModelRequestKind::Conversation, RequestOutcome::Accepted),
            (
                ModelRequestKind::WebFetch,
                RequestOutcome::Failed(RequestFailure::Http { status: 503 })
            ),
            (ModelRequestKind::WebFetch, RequestOutcome::Accepted),
            (ModelRequestKind::Conversation, RequestOutcome::Accepted),
        ]
    );
}

#[tokio::test]
async fn interrupting_the_page_request_cancels_it_without_changing_the_conversation() {
    let (url, _) = server(vec![response(
        200,
        "Content-Type: text/plain\r\n",
        "Delivery is Friday.",
    )])
    .await;
    let entered = Arc::new(Notify::new());
    let model = WebModel::new(
        vec![fetch_call(&url, "When is delivery?")],
        PageAnswer::Pending(entered.clone()),
    );
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read the delivery page".into())
        .await
        .unwrap();
    let permission = loop {
        if let EventKind::PermissionRequested { request } =
            tokio::time::timeout(EVENT_TIMEOUT, events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
        {
            break request;
        }
    };
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(EVENT_TIMEOUT, entered.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    loop {
        match tokio::time::timeout(EVENT_TIMEOUT, events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Interrupted => break,
            EventKind::Completed { .. } => panic!("an interrupted fetch must not complete"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        request_kinds(&snapshot).contains(&(ModelRequestKind::WebFetch, RequestOutcome::Cancelled))
    );
    assert!(!conversation_mentions(&snapshot, "Web page content"));
    assert_eq!(model.page_prompts().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_redirect_to_a_new_origin_asks_for_a_new_approved_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!("http://{}/private", listener.local_addr().unwrap());
    let (url, _) = server(vec![response(
        307,
        &format!("Location: {destination}\r\n"),
        "",
    )])
    .await;
    let model = WebModel::new(
        vec![fetch_call(&format!("{url}/start"), "Summarize")],
        PageAnswer::Text("unused"),
    );
    let run = run_web(model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(!run.results[0].is_error);
    assert_eq!(
        run.results[0].content,
        format!(
            "REDIRECT DETECTED: The URL redirects to a different host.\n\nOriginal URL: {url}/start\nRedirect URL: {destination}\nStatus: 307 Temporary Redirect\n\nTo complete your request, I need to fetch content from the redirected URL. Please use WebFetch again with these parameters:\n- url: \"{destination}\"\n- prompt: \"Summarize\""
        )
    );
    assert!(model.page_prompts().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

fn domain_rule(decision: Decision, domain: &str) -> PermissionRule {
    PermissionRule {
        id: uuid::Uuid::new_v4(),
        decision,
        tool: "WebFetch".into(),
        specifier: Some(format!("domain:{domain}")),
        input: None,
        loop_id: None,
        persistent: false,
    }
}

fn plain_page(body: &str) -> String {
    response(200, "Content-Type: text/plain\r\n", body)
}

#[tokio::test]
async fn allowing_a_fetch_for_the_session_covers_later_fetches_of_that_domain_only() {
    let (url, connections) = server(vec![plain_page("Delivery is Friday.")]).await;
    let other_domain = url.replace("127.0.0.1", "localhost");
    let model = WebModel::new(
        vec![
            fetch_call(&format!("{url}/schedule"), "When is delivery?"),
            fetch_call(&format!("{url}/orders?page=2"), "How many orders are open?"),
            fetch_call(&format!("{other_domain}/schedule"), "When is delivery?"),
        ],
        PageAnswer::Text("Friday."),
    );
    let run = run_web_with(model, None, Vec::new(), |request| {
        if request.input["url"].as_str().unwrap().contains("localhost") {
            Reply::Deny
        } else {
            Reply::Allow(PermissionScope::Session)
        }
    })
    .await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(
        run.permission_requests,
        vec![
            (
                "WebFetch".to_owned(),
                json!({"url": format!("{url}/schedule"), "prompt": "When is delivery?"})
            ),
            (
                "WebFetch".to_owned(),
                json!({"url": format!("{other_domain}/schedule"), "prompt": "When is delivery?"})
            ),
        ]
    );
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    assert_eq!(run.results.len(), 3);
    assert!(run.results[..2].iter().all(|result| !result.is_error));
    assert!(run.results[2].is_error);

    let mut identities = run
        .approvals
        .iter()
        .map(|record| (record.outcome, record.input_summary.text.clone()))
        .collect::<Vec<_>>();
    identities.sort_by_key(|(outcome, _)| *outcome == ApprovalResult::Deny);
    assert_eq!(
        identities,
        vec![
            (
                ApprovalResult::Allow,
                "WebFetch: domain:127.0.0.1; path and prompt omitted".to_owned()
            ),
            (
                ApprovalResult::Allow,
                "WebFetch: domain:127.0.0.1; path and prompt omitted".to_owned()
            ),
            (
                ApprovalResult::Deny,
                "WebFetch: domain:localhost; path and prompt omitted".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn a_saved_rule_allows_the_whole_domain_from_then_on() {
    for uses_suggested_rule in [true, false] {
        let (url, _) = server(vec![plain_page("Delivery is Friday.")]).await;
        let model = WebModel::new(
            vec![
                fetch_call(&format!("{url}/schedule"), "When is delivery?"),
                fetch_call(&format!("{url}/orders"), "How many orders are open?"),
            ],
            PageAnswer::Text("Friday."),
        );
        let run = run_web_with(model, None, Vec::new(), |_| {
            if uses_suggested_rule {
                Reply::AllowWithSuggestedRule
            } else {
                Reply::Allow(PermissionScope::Persistent)
            }
        })
        .await;
        assert!(run.failure.is_none(), "{:?}", run.failure);
        assert_eq!(run.permission_requests.len(), 1);
        assert!(run.results.iter().all(|result| !result.is_error));
        let [rule] = run.snapshot.policy.rules.as_slice() else {
            panic!("one rule was saved: {:?}", run.snapshot.policy.rules);
        };
        assert_eq!(
            (rule.decision, rule.tool.as_str(), rule.specifier.as_deref()),
            (Decision::Allow, "WebFetch", Some("domain:127.0.0.1"))
        );
        assert!(rule.input.is_none());
    }
}

#[tokio::test]
async fn a_denied_domain_is_refused_in_claude_code_words_without_contacting_it() {
    let (url, connections) = server(vec![plain_page("Delivery is Friday.")]).await;
    let model = WebModel::new(
        vec![fetch_call(&format!("{url}/schedule"), "When is delivery?")],
        PageAnswer::Text("unused"),
    );
    let run = run_web_with(
        model,
        None,
        vec![domain_rule(Decision::Deny, "127.0.0.1")],
        |_| Reply::Deny,
    )
    .await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(run.permission_requests.is_empty());
    assert!(run.results[0].is_error);
    assert_eq!(
        run.results[0].content,
        "WebFetch denied access to domain:127.0.0.1."
    );
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    let [denial] = run.approvals.as_slice() else {
        panic!("one record: {:?}", run.approvals);
    };
    assert_eq!(denial.outcome, ApprovalResult::Deny);
    assert_eq!(
        denial.input_summary.text,
        "WebFetch: domain:127.0.0.1; path and prompt omitted"
    );
    assert_eq!(
        denial
            .matching_rules
            .iter()
            .map(|rule| rule.matcher.text.as_str())
            .collect::<Vec<_>>(),
        ["WebFetch(domain:127.0.0.1)"]
    );
}

#[tokio::test]
async fn a_redirect_to_another_domain_is_judged_by_the_new_domain() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!(
        "http://localhost:{}/private",
        listener.local_addr().unwrap().port()
    );
    let (url, _) = server(vec![response(
        307,
        &format!("Location: {destination}\r\n"),
        "",
    )])
    .await;
    let model = WebModel::new(
        vec![
            fetch_call(&format!("{url}/start"), "Summarize"),
            fetch_call(&destination, "Summarize"),
        ],
        PageAnswer::Text("unused"),
    );
    let run = run_web_with(
        model,
        None,
        vec![domain_rule(Decision::Allow, "127.0.0.1")],
        |_| Reply::Deny,
    )
    .await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(
        run.results[0]
            .content
            .starts_with("REDIRECT DETECTED: The URL redirects to a different host."),
        "{}",
        run.results[0].content
    );
    assert_eq!(
        run.permission_requests,
        vec![(
            "WebFetch".to_owned(),
            json!({"url": destination, "prompt": "Summarize"})
        )]
    );
    assert!(run.results[1].is_error);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_long_page_is_truncated_before_the_model_reads_it() {
    let kept = "Delivery confirmed. ".repeat(5_000);
    let (url, _) = server(vec![response(
        200,
        "Content-Type: text/plain\r\n",
        &format!("{kept}DROPPED TAIL"),
    )])
    .await;
    let model = WebModel::new(
        vec![fetch_call(&url, "When is delivery?")],
        PageAnswer::Text("Confirmed."),
    );
    let run = run_web(model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(run.results[0].content, "Confirmed.");
    let prompts = model.page_prompts();
    assert!(prompts[0].starts_with(&format!(
        "{PAGE_PROMPT_START}{kept}\n\n[Content truncated due to length...]\n---\n\nWhen is delivery?"
    )));
    assert!(!prompts[0].contains("DROPPED TAIL"));
}

fn search_config(url: &str) -> SearchConfig {
    SearchConfig {
        provider: tickloop_harness::protocol::SearchProviderKind::Searxng,
        endpoint: format!("{url}/search"),
        api_key_env: None,
    }
}

/// Turning search off or on keeps WebSearch listed and the request prefix byte for byte the same;
/// a call while it is off explains how to turn it on, without review or approval in Auto mode.
#[tokio::test]
async fn search_off_keeps_the_prefix_and_answers_a_call_with_how_to_enable_it() {
    let directory = tempfile::tempdir().unwrap();
    let model = WebModel::new(
        vec![("WebSearch", json!({"query": "rust"}))],
        PageAnswer::Text("unused"),
    );
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, tickloop_harness::protocol::ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let mut results = Vec::new();
    for enabled in [false, true, false] {
        agent
            .set_search(chat, enabled.then(SearchConfig::default))
            .await
            .unwrap();
        agent
            .submit(chat, "Check web search availability".into())
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(EVENT_TIMEOUT, events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::ToolCompleted { result, .. } => results.push(result),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                EventKind::PermissionRequested { .. } => panic!("no approval was needed"),
                _ => {}
            }
        }
    }
    let disabled = tickloop_harness::tools::web_search::DISABLED_MESSAGE;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert_eq!(results[0].content, disabled);
    for expected in [
        "/websearch",
        "bypass this setting",
        "WebFetch remains available",
    ] {
        assert!(disabled.contains(expected), "{expected}");
    }
    assert!(model.page_requests.lock().unwrap().is_empty());
    let requests = model.conversation_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&requests[0].messages[..2]).unwrap()
        );
        for name in ["WebSearch", "WebFetch"] {
            assert!(request.tools.iter().any(|tool| tool.name == name), "{name}");
        }
    }
    let prefix = serde_json::to_string(&requests[0].messages[..2]).unwrap();
    assert!(!prefix.contains("/websearch"));
    assert!(!prefix.contains("web_search"));
    agent.shutdown().await;
}

#[tokio::test]
async fn search_selection_is_account_independent_and_persists_for_new_agents() {
    use tickloop_harness::protocol::SearchProviderKind;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let settings = tickloop_code_settings::UserSettings::in_root(&root);
    let agent = Agent::new_with_user_settings_embedded(
        config(),
        WebModel::new(Vec::new(), PageAnswer::Text("unused")),
        settings.clone(),
    );
    let first = agent.create_chat(root.clone()).await.unwrap();
    assert_eq!(
        agent.snapshot(first).await.unwrap().search_config,
        SearchConfig::enabled_default()
    );
    let chosen = SearchConfig::for_provider(SearchProviderKind::Tavily);
    agent.set_search(first, Some(chosen.clone())).await.unwrap();
    assert_eq!(
        agent.snapshot(first).await.unwrap().search_config,
        Some(chosen.clone())
    );
    let second = agent.create_chat(root.clone()).await.unwrap();
    assert_eq!(
        agent.snapshot(second).await.unwrap().search_config,
        Some(chosen)
    );
    agent.set_search(second, None).await.unwrap();
    let third = agent.create_chat(root).await.unwrap();
    assert_eq!(agent.snapshot(third).await.unwrap().search_config, None);
    assert_eq!(settings.load().search_config(), None);
    agent.shutdown().await;
}

fn search_response(results: Value) -> Vec<String> {
    vec![response(
        200,
        "Content-Type: application/json\r\n",
        &json!({"results": results}).to_string(),
    )]
}

#[tokio::test]
async fn configured_search_freezes_the_approved_source_and_returns_claude_code_result_text() {
    let (url, _) = server(search_response(json!([{"title":"Rust documentation","url":"https://doc.rust-lang.org/","content":"Official language documentation"}]))).await;
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        WebModel::new(
            vec![("WebSearch", json!({"query":"Rust documentation"}))],
            PageAnswer::Text("unused"),
        ),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_search(chat, Some(search_config(&url)))
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Search for Rust documentation".into())
        .await
        .unwrap();
    let mut saw_result = false;
    loop {
        match tokio::time::timeout(EVENT_TIMEOUT, events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.name, "WebSearch");
                assert_eq!(request.input["endpoint"], format!("{url}/search"));
                agent
                    .set_search(chat, Some(search_config("http://127.0.0.1:1")))
                    .await
                    .unwrap();
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted { result, .. } => {
                assert!(!result.is_error, "{}", result.content);
                assert!(
                    result
                        .content
                        .starts_with("Search source: SearXNG / compatible JSON\nSearched at: ")
                );
                assert!(
                    result.content.contains(&
                    format!(
                        "Web search results for query: \"Rust documentation\"\n\nLinks: [{{\"title\":\"Rust documentation\",\"url\":\"https://doc.rust-lang.org/\",\"summary\":\"Official language documentation\"}}]{SOURCES_REMINDER}"
                    ))
                );
                saw_result = true;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(saw_result);
    agent.shutdown().await;
}

#[tokio::test]
async fn search_distinguishes_disabled_empty_rate_limited_and_invalid_responses() {
    let search = || {
        WebModel::new(
            vec![("WebSearch", json!({"query":"rust"}))],
            PageAnswer::Text("unused"),
        )
    };
    let disabled_model = search();
    let run = run_web(disabled_model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(run.results[0].is_error);
    assert_eq!(
        run.results[0].content,
        tickloop_harness::tools::web_search::DISABLED_MESSAGE
    );
    assert!(run.permission_requests.is_empty());
    assert!(
        disabled_model
            .conversation_requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.tools.iter().any(|tool| tool.name == "WebSearch"))
    );
    for (status, body, expected) in [
        (200, "{\"results\":[]}", None),
        (429, "busy", Some("rate limited")),
        (200, "not JSON", Some("invalid JSON")),
        (403, "denied", Some("access denied")),
    ] {
        let (url, _) = server(vec![response(
            status,
            "Content-Type: application/json\r\n",
            body,
        )])
        .await;
        let run = run_web(search(), Some(search_config(&url))).await;
        assert!(run.failure.is_none(), "{:?}", run.failure);
        let result = &run.results[0];
        match expected {
            Some(expected) => assert!(result.is_error && result.content.contains(expected)),
            None => assert!(result.content.contains(&format!(
                "Web search results for query: \"rust\"\n\nNo links found.{SOURCES_REMINDER}"
            ))),
        }
    }
}

#[tokio::test]
async fn search_results_are_filtered_by_host_including_subdomains() {
    let results = json!([
        {"title":"Standard library","url":"https://doc.rust-lang.org/std/"},
        {"title":"Rust","url":"https://www.rust-lang.org/"},
        {"title":"Lookalike","url":"https://notrust-lang.org/"},
        {"title":"Example","url":"https://example.com/rust"}
    ]);
    for (filter, expected) in [
        (
            json!({"allowed_domains": ["rust-lang.org"]}),
            vec![
                "https://doc.rust-lang.org/std/",
                "https://www.rust-lang.org/",
            ],
        ),
        (
            json!({"blocked_domains": ["rust-lang.org"]}),
            vec!["https://notrust-lang.org/", "https://example.com/rust"],
        ),
    ] {
        let (url, _) = server(search_response(results.clone())).await;
        let mut input = json!({"query": "rust"});
        input
            .as_object_mut()
            .unwrap()
            .extend(filter.as_object().unwrap().clone());
        let run = run_web(
            WebModel::new(vec![("WebSearch", input)], PageAnswer::Text("unused")),
            Some(search_config(&url)),
        )
        .await;
        assert!(run.failure.is_none(), "{:?}", run.failure);
        let links = run.results[0]
            .content
            .lines()
            .find_map(|line| line.strip_prefix("Links: "))
            .unwrap();
        let links: Vec<Value> = serde_json::from_str(links).unwrap();
        let urls = links
            .iter()
            .map(|link| link["url"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(urls, expected);
    }
}

#[tokio::test]
async fn both_domain_lists_are_rejected_before_approval_or_search() {
    let (url, connections) = server(search_response(json!([]))).await;
    let run = run_web(
        WebModel::new(
            vec![(
                "WebSearch",
                json!({"query":"rust","allowed_domains":["rust-lang.org"],"blocked_domains":["example.com"]}),
            )],
            PageAnswer::Text("unused"),
        ),
        Some(search_config(&url)),
    )
    .await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(run.permission_requests.is_empty());
    assert!(run.results[0].is_error);
    assert_eq!(
        run.results[0].content,
        "Error: Cannot specify both allowed_domains and blocked_domains in the same request"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[ignore = "uses the real public Mwmbl search API and Rust documentation website"]
async fn live_mwmbl_search_and_official_web_page_through_the_agent() {
    let run = run_web(
        WebModel::new(
            vec![("WebSearch", json!({"query":"rust documentation"}))],
            PageAnswer::Text("unused"),
        ),
        Some(SearchConfig {
            provider: tickloop_harness::protocol::SearchProviderKind::Searxng,
            endpoint: "https://api.mwmbl.org/api/v2/search/".into(),
            api_key_env: None,
        }),
    )
    .await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(!run.results[0].is_error, "{}", run.results[0].content);
    assert!(run.results[0].content.contains("rust-lang.org"));
    println!("real search result:\n{}", run.results[0].content);
    let model = WebModel::new(
        vec![fetch_call(
            "https://doc.rust-lang.org/",
            "What does this page offer?",
        )],
        PageAnswer::Text("The Rust documentation."),
    );
    let run = run_web(model.clone(), None).await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(!run.results[0].is_error, "{}", run.results[0].content);
    let prompts = model.page_prompts();
    assert!(prompts[0].contains("Rust"));
    println!("real page prompt: {} characters", prompts[0].len());
}
