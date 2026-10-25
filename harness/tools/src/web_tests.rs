use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

/// Records each prompt and answers with a fixed text.
#[derive(Default)]
struct RecordingModel {
    prompts: Mutex<Vec<String>>,
}

impl RecordingModel {
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl SecondaryModel for RecordingModel {
    async fn respond(&self, prompt: String, _: CancellationToken) -> Result<String, ToolError> {
        self.prompts.lock().unwrap().push(prompt);
        Ok("The delivery is on Friday.".into())
    }
}

fn http_response(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Serves `responses` in order, repeating the last one, and counts the connections it accepts.
async fn serve(responses: Vec<Vec<u8>>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let index = accepted.fetch_add(1, Ordering::SeqCst);
            let response = responses[index.min(responses.len() - 1)].clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(length) => request.extend_from_slice(&buffer[..length]),
                    }
                }
                let _ = socket.write_all(&response).await;
            });
        }
    });
    (format!("http://{address}"), connections)
}

fn fetch_input(url: &str, prompt: &str) -> FetchInput {
    let (_, operation, _) = prepare_fetch(&json!({"url": url, "prompt": prompt})).unwrap();
    let Operation::WebFetch(input) = operation else {
        panic!("WebFetch prepares a fetch");
    };
    input
}

fn search_input(input: Value) -> Result<(SearchInput, Value), ToolError> {
    let (_, operation, normalized) = prepare_search(&input)?;
    let Operation::WebSearch(search) = operation else {
        panic!("WebSearch prepares a search");
    };
    Ok((search, normalized))
}

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

#[test]
fn web_tools_use_claude_code_names_and_parameters() {
    let names = super::super::Tools::definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&WEB_FETCH_TOOL_NAME.to_owned()));
    assert!(names.contains(&WEB_SEARCH_TOOL_NAME.to_owned()));
    assert!(
        !names
            .iter()
            .any(|name| name == "web_fetch" || name == "web_search")
    );

    let fetch = fetch_definition();
    assert_eq!(fetch.input_schema["required"], json!(["url", "prompt"]));
    assert!(fetch.description.contains("15-minute cache"));
    assert!(fetch.description.contains("via Bash instead"));
    let search = search_definition();
    assert_eq!(search.input_schema["required"], json!(["query"]));
    assert_eq!(search.input_schema["properties"]["query"]["minLength"], 2);
    for property in ["allowed_domains", "blocked_domains"] {
        assert!(search.input_schema["properties"][property].is_object());
    }
    assert!(search.description.contains("The current month is "));
    assert!(search.description.contains("\"Sources:\" section"));

    for input in [
        json!({"url": "https://example.com"}),
        json!({"url": "https://example.com", "prompt": "Summarize", "timeout_ms": 20}),
    ] {
        assert!(matches!(
            prepare_fetch(&input),
            Err(ToolError::InvalidInput(_))
        ));
    }
}

#[test]
fn fetch_urls_are_limited_parsed_and_upgraded_to_https() {
    let prefix = "https://example.com/";
    let longest = format!("{prefix}{}", "a".repeat(MAX_URL_LENGTH - prefix.len()));
    assert!(prepare_fetch(&json!({"url": longest, "prompt": "p"})).is_ok());
    let too_long = format!("{longest}a");
    assert!(matches!(
        prepare_fetch(&json!({"url": too_long, "prompt": "p"})),
        Err(ToolError::InvalidInput(message)) if message.contains("2000")
    ));

    let Err(ToolError::Rejected(message)) =
        prepare_fetch(&json!({"url": "not a url", "prompt": "p"}))
    else {
        panic!("an unparsable URL is rejected");
    };
    assert_eq!(
        message,
        "Error: Invalid URL \"not a url\". The URL provided could not be parsed."
    );

    let (path, _, normalized) =
        prepare_fetch(&json!({"url": "http://example.com/page?q=1", "prompt": "Summarize"}))
            .unwrap();
    assert_eq!(path, ".");
    assert_eq!(
        normalized,
        json!({"url": "https://example.com/page?q=1", "prompt": "Summarize"})
    );
    let (_, _, loopback) =
        prepare_fetch(&json!({"url": "http://127.0.0.1:8080/page", "prompt": "p"})).unwrap();
    assert_eq!(loopback["url"], "http://127.0.0.1:8080/page");

    for rejected in ["https://user:secret@example.com/", "ftp://example.com/file"] {
        assert!(matches!(
            prepare_fetch(&json!({"url": rejected, "prompt": "p"})),
            Err(ToolError::InvalidInput(_))
        ));
    }
}

#[tokio::test]
async fn html_becomes_markdown_without_scripts_or_styles_and_other_text_passes_through() {
    let html = "<html><head><style>hidden style</style></head><body><h1>Delivery update</h1><p>Delivery is <b>Friday</b> &amp; confirmed.</p><ul><li><a href=\"https://example.com/terms\">Terms</a></li></ul><script>hidden script</script><noscript>hidden fallback</noscript></body></html>";
    let markdown = page_markdown("text/html; charset=utf-8", html.as_bytes().to_vec())
        .await
        .unwrap();
    assert!(markdown.contains("# Delivery update"), "{markdown}");
    assert!(
        markdown.contains("Delivery is **Friday** & confirmed."),
        "{markdown}"
    );
    assert!(
        markdown.contains("[Terms](https://example.com/terms)"),
        "{markdown}"
    );
    assert!(!markdown.contains("hidden"), "{markdown}");

    let xhtml = page_markdown(
        "application/xhtml+xml",
        b"<p>Plain <em>text</em></p>".to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(xhtml, "Plain *text*");

    for (content_type, body) in [
        ("text/plain", "<h1>not converted</h1>\n  spacing kept\n"),
        ("text/markdown", "# Heading\n\n- item\n"),
        ("application/json", "{\"price\": 42}"),
    ] {
        assert_eq!(
            page_markdown(content_type, body.as_bytes().to_vec())
                .await
                .unwrap(),
            body
        );
    }

    assert!(matches!(
        page_markdown("image/png", vec![0x89, b'P', b'N', b'G']).await,
        Err(ToolError::Network(message)) if message.contains("content type")
    ));
    assert!(matches!(
        page_markdown("text/plain", vec![0xff, 0xfe]).await,
        Err(ToolError::Network(message)) if message.contains("UTF-8")
    ));
}

#[test]
fn markdown_beyond_the_limit_is_truncated_with_a_notice() {
    let exact = "a".repeat(MAX_MARKDOWN_LENGTH);
    assert!(matches!(truncate_markdown(&exact), Cow::Borrowed(text) if text == exact));

    let longer = format!("{exact}b");
    assert_eq!(
        truncate_markdown(&longer),
        format!("{exact}\n\n[Content truncated due to length...]")
    );

    let mut surrogate_pair_at_limit = "a".repeat(MAX_MARKDOWN_LENGTH - 1);
    surrogate_pair_at_limit.push('😀');
    assert_eq!(
        truncate_markdown(&surrogate_pair_at_limit),
        format!(
            "{}\n\n[Content truncated due to length...]",
            "a".repeat(MAX_MARKDOWN_LENGTH - 1)
        )
    );
}

#[test]
fn the_secondary_prompt_is_claude_codes_prompt_for_domains_that_are_not_preapproved() {
    assert_eq!(
        secondary_model_prompt("# Page\n\nBody", "When is delivery?"),
        "
Web page content:
---
# Page

Body
---

When is delivery?

Provide a concise response based only on the content above. In your response:
 - Enforce a strict 125-character maximum for quotes from any source document. Open Source Software is ok as long as we respect the license.
 - Use quotation marks for exact language from articles; any language outside of the quotation should never be word-for-word the same.
 - You are not a lawyer and never comment on the legality of your own prompts and responses.
 - Never produce or reproduce exact song lyrics.
"
    );
}

#[test]
fn cached_pages_expire_after_fifteen_minutes() {
    let mut cache = PageCache::default();
    let stored_at = Instant::now();
    cache.insert("https://example.com/", "page".into(), stored_at);
    let almost_expired = stored_at + CACHE_TTL - Duration::from_millis(1);
    assert_eq!(
        cache.get("https://example.com/", almost_expired).as_deref(),
        Some("page")
    );
    assert!(
        cache
            .get("https://example.com/", stored_at + CACHE_TTL)
            .is_none()
    );
    assert!(cache.pages.is_empty());
    assert_eq!(cache.total_bytes, 0);
}

#[test]
fn the_cache_drops_the_least_recently_used_pages_beyond_its_budget() {
    let mut cache = PageCache::with_capacity(10);
    let now = Instant::now();
    cache.insert("a", "aaaa".into(), now);
    cache.insert("b", "bbbb".into(), now);
    assert!(cache.get("a", now).is_some());
    cache.insert("c", "cccc".into(), now);
    assert!(cache.get("b", now).is_none());
    assert!(cache.get("a", now).is_some());
    assert!(cache.get("c", now).is_some());
    assert_eq!(cache.total_bytes, 8);

    cache.insert("too large", "x".repeat(11).into(), now);
    assert!(cache.get("too large", now).is_none());
    assert_eq!(cache.total_bytes, 8);

    cache.insert("a", "replaced".into(), now);
    assert_eq!(cache.get("a", now).as_deref(), Some("replaced"));
    assert!(cache.get("c", now).is_none());
    assert_eq!(cache.total_bytes, 8);

    let later = now + CACHE_TTL;
    cache.insert("d", "dd".into(), later);
    assert_eq!(cache.pages.len(), 1);
    assert_eq!(cache.total_bytes, 2);
}

#[tokio::test]
async fn a_cached_page_is_answered_again_without_a_second_download() {
    let (base, connections) = serve(vec![http_response(
        "200 OK",
        "Content-Type: text/html\r\n",
        "<h1>Delivery</h1><p>Friday</p>",
    )])
    .await;
    let cache = FetchCache::default();
    let model = RecordingModel::default();
    let page = format!("{base}/delivery");
    for prompt in ["When is delivery?", "Who confirmed it?"] {
        let result = fetch(
            fetch_input(&page, prompt),
            &cache,
            &Observations::default(),
            Reach::Anywhere,
            &model,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.web_sources[0].title, "Delivery");
        let result = result.result.unwrap();
        assert_eq!(result.content, "The delivery is on Friday.");
        assert!(!result.is_error);
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(
        prompts[1],
        secondary_model_prompt("# Delivery\n\nFriday", "Who confirmed it?")
    );
}

#[tokio::test]
async fn a_cross_origin_redirect_asks_for_a_new_call_without_contacting_the_target() {
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!("http://{}/private", target.local_addr().unwrap());
    let (base, _) = serve(vec![
        http_response("302 Found", "Location: /moved\r\n", ""),
        http_response(
            "301 Moved Permanently",
            &format!("Location: {destination}\r\n"),
            "",
        ),
    ])
    .await;
    let model = RecordingModel::default();
    let result = fetch(
        fetch_input(&format!("{base}/start"), "Summarize \"it\""),
        &FetchCache::default(),
        &Observations::default(),
        Reach::Anywhere,
        &model,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(result.web_sources.is_empty());
    let result = result.result.unwrap();
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        format!(
            "REDIRECT DETECTED: The URL redirects to a different host.

Original URL: {base}/moved
Redirect URL: {destination}
Status: 301 Moved Permanently

To complete your request, I need to fetch content from the redirected URL. Please use WebFetch again with these parameters:
- url: \"{destination}\"
- prompt: \"Summarize \"it\"\""
        )
    );
    assert!(model.prompts().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), target.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn responses_over_ten_megabytes_are_rejected() {
    let declared = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        MAX_RESPONSE_BYTES + 1
    )
    .into_bytes();
    let mut streamed =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
    streamed.extend(std::iter::repeat_n(b'a', MAX_RESPONSE_BYTES + 1));
    for response in [declared, streamed] {
        let (base, _) = serve(vec![response]).await;
        let error = fetch_page(url(&base), FETCH_TIMEOUT, Reach::Anywhere)
            .await
            .err()
            .expect("an oversized response is rejected");
        assert!(error.to_string().contains("exceeds 10 MB"), "{error}");
    }
    let mut largest =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
    largest.extend(std::iter::repeat_n(b'a', MAX_RESPONSE_BYTES));
    let (base, _) = serve(vec![largest]).await;
    assert!(matches!(
        fetch_page(url(&base), FETCH_TIMEOUT, Reach::Anywhere).await,
        Ok(FetchedPage::Markdown(markdown)) if markdown.len() == MAX_RESPONSE_BYTES
    ));
}

#[tokio::test]
async fn a_silent_server_times_out_and_cancellation_stops_before_the_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let silent = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let error = fetch_page(
        url(&format!("http://{address}/slow")),
        Duration::from_millis(50),
        Reach::Anywhere,
    )
    .await
    .err()
    .expect("a silent server times out");
    assert_eq!(error.to_string(), "network tool failed: request timed out");
    silent.abort();

    let (base, connections) = serve(vec![http_response("200 OK", "", "unused")]).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        fetch(
            fetch_input(&base, "p"),
            &FetchCache::default(),
            &Observations::default(),
            Reach::Anywhere,
            &RecordingModel::default(),
            cancel,
        )
        .await,
        Err(ToolError::Cancelled)
    ));
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[test]
fn search_input_follows_claude_code_validation() {
    assert!(matches!(
        search_input(json!({"query": "a"})),
        Err(ToolError::InvalidInput(message)) if message.contains("at least 2")
    ));
    assert!(matches!(
        search_input(json!({"query": "  "})),
        Err(ToolError::InvalidInput(_))
    ));
    let Err(ToolError::Rejected(message)) = search_input(json!({
        "query": "rust",
        "allowed_domains": ["rust-lang.org"],
        "blocked_domains": ["example.com"]
    })) else {
        panic!("both domain lists are rejected");
    };
    assert_eq!(
        message,
        "Error: Cannot specify both allowed_domains and blocked_domains in the same request"
    );
    for limit in [json!(0), json!(51), json!("many")] {
        assert!(search_input(json!({"query": "rust", "limit": limit})).is_err());
    }
    for domain in [
        "example.com/blog",
        "https://example.com",
        "user@example.com",
        "",
    ] {
        assert!(matches!(
            search_input(json!({"query": "rust", "blocked_domains": [domain]})),
            Err(ToolError::InvalidInput(message)) if message.contains("blocked_domains")
        ));
    }
    assert!(search_input(json!({"query": "rust", "background": true})).is_err());

    let (search, normalized) = search_input(json!({
        "query": "rust",
        "allowed_domains": ["*.Rust-Lang.ORG.", " docs.rs "],
        "blocked_domains": []
    }))
    .unwrap();
    assert_eq!(
        normalized,
        json!({"query": "rust", "allowed_domains": ["rust-lang.org", "docs.rs"]})
    );
    assert_eq!(
        search.domains,
        DomainFilter::Allowed(vec!["rust-lang.org".into(), "docs.rs".into()])
    );
    let (search, normalized) =
        search_input(json!({"query": "rust", "allowed_domains": null})).unwrap();
    assert_eq!(normalized, json!({"query": "rust"}));
    assert_eq!(search.domains, DomainFilter::Any);
}

#[test]
fn domain_filters_match_result_hosts_and_their_subdomains() {
    let allowed = DomainFilter::Allowed(vec!["rust-lang.org".into()]);
    let blocked = DomainFilter::Blocked(vec!["rust-lang.org".into()]);
    for (address, is_listed) in [
        ("https://rust-lang.org/", true),
        ("https://doc.rust-lang.org/std/", true),
        ("https://DOC.Rust-Lang.org./book", true),
        ("https://notrust-lang.org/", false),
        ("https://rust-lang.org.example.com/", false),
        ("https://example.com/?next=rust-lang.org", false),
    ] {
        assert_eq!(allowed.admits(&url(address)), is_listed, "{address}");
        assert_eq!(blocked.admits(&url(address)), !is_listed, "{address}");
        assert!(DomainFilter::Any.admits(&url(address)));
    }
}

#[test]
fn search_results_use_claude_codes_result_text() {
    let links = vec![
        SearchLink {
            published_at: None,
            title: "Rust documentation".into(),
            url: "https://doc.rust-lang.org/".into(),
            summary: "Official \"docs\"".into(),
        },
        SearchLink {
            published_at: None,
            title: "Docs.rs".into(),
            url: "https://docs.rs/".into(),
            summary: String::new(),
        },
    ];
    assert_eq!(
        search_result_text("rust docs", &links),
        "Web search results for query: \"rust docs\"

Links: [{\"title\":\"Rust documentation\",\"url\":\"https://doc.rust-lang.org/\",\"summary\":\"Official \\\"docs\\\"\"},{\"title\":\"Docs.rs\",\"url\":\"https://docs.rs/\"}]


REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."
    );
    assert_eq!(
        search_result_text("rust docs", &[]),
        "Web search results for query: \"rust docs\"

No links found.


REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."
    );
}

#[tokio::test]
async fn a_public_only_fetch_never_connects_to_a_name_that_resolves_to_loopback() {
    let (base, connections) = serve(vec![http_response("200 OK", "", "private")]).await;
    let port = url(&base).port().unwrap();
    for (target, refusal) in [
        (
            format!("http://localhost:{port}/"),
            "localhost resolves to ",
        ),
        (base.clone(), "127.0.0.1 is not a public address"),
    ] {
        let error = fetch_page(url(&target), FETCH_TIMEOUT, Reach::PublicAddresses)
            .await
            .err()
            .expect("a loopback page is refused");
        assert!(
            matches!(&error, ToolError::Rejected(message) if message.starts_with(refusal)),
            "{target}: {error}"
        );
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[test]
fn a_public_only_fetch_refuses_redirects_to_addresses_that_are_not_public() {
    let page = url("https://documents.example.com/start");
    for location in [
        "http://127.0.0.1:8080/",
        "https://10.0.0.1/admin",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/",
        "https://[fd00::1]/",
        "https://[::ffff:192.168.1.1]/",
        "https://[2002:c0a8:101::1]/",
    ] {
        assert!(
            matches!(
                redirect_target(&page, location, Reach::PublicAddresses),
                Err(ToolError::Rejected(_))
            ),
            "{location}"
        );
        assert!(redirect_target(&page, location, Reach::Anywhere).is_ok());
    }
    for location in [
        "/next",
        "https://8.8.8.8/",
        "https://[2606:4700:4700::1111]/",
    ] {
        assert!(redirect_target(&page, location, Reach::PublicAddresses).is_ok());
    }
}

#[tokio::test]
async fn folderless_tools_prepare_only_web_fetch_of_public_addresses_and_write_nothing() {
    let tools = crate::Tools::folderless(Reach::PublicAddresses);
    let workspace = crate::Workspace {
        folder: None,
        additional_directories: Vec::new(),
    };
    assert!(tools.chat_directory().is_none());
    assert!(tools.family_temp_directory().is_none());
    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace,
            WEB_FETCH_TOOL_NAME,
            &json!({"url": "https://example.com/a", "prompt": "p"}),
        )
        .await
        .unwrap();
    assert!(prepared.requires_permission && prepared.is_read_only && !prepared.in_workspace);
    for url in [
        "http://127.0.0.1/",
        "https://192.168.0.1/",
        "https://[::1]/",
        "https://[2002:7f00:1::]/",
    ] {
        assert!(
            matches!(
                tools
                    .prepare(
                        &tickloop_ids::IdContext::new(),
                        &workspace,
                        WEB_FETCH_TOOL_NAME,
                        &json!({"url": url, "prompt": "p"})
                    )
                    .await,
                Err(ToolError::Rejected(_))
            ),
            "{url}"
        );
    }
    for (name, input) in [
        (crate::READ_TOOL_NAME, json!({"file_path": "/etc/hosts"})),
        (crate::GLOB_TOOL_NAME, json!({"pattern": "*"})),
        (
            crate::WRITE_TOOL_NAME,
            json!({"file_path": "/tmp/x", "content": "x"}),
        ),
        (BASH_TOOL_NAME, json!({"command": "true"})),
    ] {
        assert!(
            matches!(
                tools.prepare(&tickloop_ids::IdContext::new(), &workspace, name, &input).await,
                Err(ToolError::Rejected(message)) if message.contains("no Folder")
            ),
            "{name}"
        );
    }
}

#[tokio::test]
async fn folderless_tools_bind_web_search_to_the_chats_search_service() {
    let workspace = crate::Workspace {
        folder: None,
        additional_directories: Vec::new(),
    };
    let input = json!({"query": "rust release notes"});
    let config = tickloop_harness_protocol::SearchConfig {
        provider: tickloop_harness_protocol::SearchProviderKind::Searxng,
        endpoint: "http://127.0.0.1:8888/search".into(),
        api_key_env: None,
    };
    let tools = crate::Tools::folderless(Reach::PublicAddresses)
        .with_search_configuration(Some(config), None);
    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace,
            WEB_SEARCH_TOOL_NAME,
            &input,
        )
        .await
        .unwrap();
    assert!(prepared.requires_permission && !prepared.in_workspace);
    assert_eq!(prepared.input["endpoint"], "http://127.0.0.1:8888/search");
    assert!(matches!(
        &prepared.operation,
        crate::Operation::WebSearch(search) if search.prepared.is_some()
    ));

    let disabled =
        crate::Tools::folderless(Reach::PublicAddresses).with_search_configuration(None, None);
    assert!(matches!(
        disabled.prepare(&tickloop_ids::IdContext::new(), &workspace, WEB_SEARCH_TOOL_NAME, &input).await,
        Err(ToolError::Network(message)) if message == web_search::DISABLED_MESSAGE
    ));
}

#[tokio::test]
async fn folderless_tools_fetch_a_private_address_only_when_the_host_chooses_anywhere() {
    let (base, connections) = serve(vec![http_response(
        "200 OK",
        "Content-Type: text/html\r\n",
        "<h1>Delivery</h1><p>Friday</p>",
    )])
    .await;
    let port = url(&base).port().unwrap();
    let workspace = crate::Workspace {
        folder: None,
        additional_directories: Vec::new(),
    };
    let input = json!({"url": format!("http://localhost:{port}/delivery"), "prompt": "When?"});
    let context = || crate::ExecutionContext {
        ids: tickloop_ids::IdContext::new(),
        operation_id: uuid::Uuid::new_v4(),
        loop_id: uuid::Uuid::nil(),
        accepts_images: false,
    };
    let model = RecordingModel::default();

    let public_only = crate::Tools::folderless(Reach::PublicAddresses);
    let prepared = public_only
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace,
            WEB_FETCH_TOOL_NAME,
            &input,
        )
        .await
        .unwrap();
    let refused = public_only
        .execute_with_context(prepared, CancellationToken::new(), context(), &model)
        .await
        .result;
    assert!(
        matches!(
            &refused,
            Err(ToolError::Rejected(message))
                if message.starts_with("localhost resolves to ")
                    && message.contains("which is not a public address")
        ),
        "{refused:?}"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 0);

    let anywhere = crate::Tools::folderless(Reach::Anywhere);
    let prepared = anywhere
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace,
            WEB_FETCH_TOOL_NAME,
            &input,
        )
        .await
        .unwrap();
    let fetched = anywhere
        .execute_with_context(prepared, CancellationToken::new(), context(), &model)
        .await
        .result
        .unwrap();
    assert!(!fetched.is_error, "{fetched:?}");
    assert_eq!(fetched.content, "The delivery is on Friday.");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

/// The search service is the host's choice, not the model's, so a Chat's reach does not limit it.
#[tokio::test]
async fn folderless_web_search_requests_the_hosts_search_service_whatever_the_reach() {
    let workspace = crate::Workspace {
        folder: None,
        additional_directories: Vec::new(),
    };
    let input = json!({"query": "rust release notes"});
    for reach in [Reach::PublicAddresses, Reach::Anywhere] {
        let (base, connections) = serve(vec![http_response(
            "200 OK",
            "Content-Type: application/json\r\n",
            r#"{"results":[{"title":"Rust 1.90","url":"https://blog.rust-lang.org/1.90","content":"Release notes"}]}"#,
        )])
        .await;
        let config = tickloop_harness_protocol::SearchConfig {
            provider: tickloop_harness_protocol::SearchProviderKind::Searxng,
            endpoint: format!("{base}/search"),
            api_key_env: None,
        };
        let tools = crate::Tools::folderless(reach).with_search_configuration(Some(config), None);
        let prepared = tools
            .prepare(
                &tickloop_ids::IdContext::new(),
                &workspace,
                WEB_SEARCH_TOOL_NAME,
                &input,
            )
            .await
            .unwrap();
        let result = tools
            .execute(prepared, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            !result.is_error && result.content.contains("https://blog.rust-lang.org/1.90"),
            "{reach:?}: {result:?}"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1, "{reach:?}");
    }
}
