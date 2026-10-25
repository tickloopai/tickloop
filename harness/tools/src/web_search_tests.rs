use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(
    status: u16,
    content_type: &str,
    body: &str,
) -> (Url, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/search", listener.local_addr().unwrap())).unwrap();
    let reply = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(socket.read_u8().await.unwrap());
        }
        let headers = String::from_utf8(bytes.clone()).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|s| s.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        bytes.extend(body);
        socket.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (url, task)
}

fn request(domains: DomainFilter) -> SearchRequest {
    SearchRequest {
        query: "rust documentation".into(),
        domains,
    }
}

fn context() -> SearchContext {
    SearchContext {
        cancel: CancellationToken::new(),
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        max_results: DEFAULT_RESULT_LIMIT,
    }
}

#[tokio::test]
async fn native_adapters_map_credentials_filters_and_results_without_a_model() {
    for kind in [
        SearchProviderKind::Brave,
        SearchProviderKind::Tavily,
        SearchProviderKind::Exa,
        SearchProviderKind::SerpApi,
        SearchProviderKind::Serper,
        SearchProviderKind::Perplexity,
    ] {
        let body = match kind {
            SearchProviderKind::Brave => {
                json!({"query": {}, "web": {"results": [{"title":"Rust", "url":"https://doc.rust-lang.org/", "description":"Reference"}, {"title":"Wrong host", "url":"https://example.org/"}]}})
            }
            SearchProviderKind::Tavily => {
                json!({"results":[{"title":"Rust", "url":"https://doc.rust-lang.org/", "content":"Reference"}, {"title":"Wrong host", "url":"https://example.org/"}]})
            }
            SearchProviderKind::SerpApi | SearchProviderKind::Serper => {
                let entries = json!([{"title":"Rust", "link":"https://doc.rust-lang.org/", "snippet":"Reference", "date":"2026-10-17"}, {"title":"Wrong host", "link":"https://example.org/"}]);
                match kind {
                    SearchProviderKind::SerpApi => json!({"organic_results": entries}),
                    _ => json!({"organic": entries}),
                }
            }
            SearchProviderKind::Perplexity => {
                json!({"results":[{"title":"Rust", "url":"https://doc.rust-lang.org/", "snippet":"Reference", "date":"2026-10-17"}, {"title":"Wrong host", "url":"https://example.org/"}]})
            }
            _ => {
                json!({"results":[{"title":"Rust", "url":"https://doc.rust-lang.org/", "highlights":["Reference"], "publishedDate":"2026-10-17T00:00:00Z"}, {"title":"Wrong host", "url":"https://example.org/"}]})
            }
        };
        let (url, received) = server(200, "application/json", &body.to_string()).await;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        credentials::save_api_key(&root, "SEARCH_FIXTURE_KEY", "fixture-original-secret").unwrap();
        let config = SearchConfig {
            provider: kind,
            endpoint: url.to_string(),
            api_key_env: Some("SEARCH_FIXTURE_KEY".into()),
        };
        let request = request(DomainFilter::Allowed(vec!["rust-lang.org".into()]));
        let prepared = prepare(&config, &request, Some(&root)).unwrap();
        assert_eq!(prepared.endpoints, vec![url.to_string()]);
        assert!(!format!("{prepared:?}").contains("secret"));
        credentials::save_api_key(&root, "SEARCH_FIXTURE_KEY", "fixture-replacement-secret")
            .unwrap();
        let result = prepared
            .execute(&request, CancellationToken::new())
            .await
            .unwrap();
        let wire = received.await.unwrap();
        assert!(wire.contains("fixture-original-secret"));
        assert!(!wire.contains("fixture-replacement-secret"));
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].snippet.as_deref(), Some("Reference"));
        assert_eq!(result.provider, kind);
        match kind {
            SearchProviderKind::Brave => {
                assert!(wire.starts_with("GET "));
                assert!(wire.contains("x-subscription-token: fixture-original-secret"));
                assert!(wire.contains("site%3Arust-lang.org"));
            }
            SearchProviderKind::Tavily => {
                assert!(wire.contains("authorization: Bearer fixture-original-secret"));
                let body: Value =
                    serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["include_domains"], json!(["rust-lang.org"]));
                assert_eq!(body["include_domains_mode"], "restrict");
                assert_eq!(body["search_depth"], "basic");
            }
            SearchProviderKind::Exa => {
                assert!(wire.contains("x-api-key: fixture-original-secret"));
                let body: Value =
                    serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["includeDomains"], json!(["rust-lang.org"]));
                assert_eq!(
                    result.results[0].published_at.as_deref(),
                    Some("2026-10-17T00:00:00Z")
                );
            }
            SearchProviderKind::SerpApi => {
                assert!(wire.starts_with("GET "));
                assert!(wire.contains("engine=google"));
                assert!(wire.contains("api_key=fixture-original-secret"));
                assert!(wire.contains("site%3Arust-lang.org"));
                assert_eq!(
                    result.results[0].published_at.as_deref(),
                    Some("2026-10-17")
                );
            }
            SearchProviderKind::Serper => {
                assert!(wire.contains("x-api-key: fixture-original-secret"));
                let body: Value =
                    serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["q"], "rust documentation site:rust-lang.org");
                assert_eq!(body["num"], DEFAULT_RESULT_LIMIT);
            }
            SearchProviderKind::Perplexity => {
                assert!(wire.contains("authorization: Bearer fixture-original-secret"));
                let body: Value =
                    serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["search_domain_filter"], json!(["rust-lang.org"]));
                assert_eq!(body["search_type"], "web");
                assert_eq!(
                    result.results[0].published_at.as_deref(),
                    Some("2026-10-17")
                );
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn additional_providers_send_blocked_domains_and_keep_empty_results_empty() {
    for kind in [
        SearchProviderKind::SerpApi,
        SearchProviderKind::Serper,
        SearchProviderKind::Perplexity,
    ] {
        let payload = match kind {
            SearchProviderKind::SerpApi => {
                json!({"search_information": {"organic_results_state":"Fully empty"}})
            }
            SearchProviderKind::Serper => json!({"organic": []}),
            _ => json!({"results": []}),
        };
        let (url, received) = server(200, "application/json", &payload.to_string()).await;
        let connection = Connection {
            url,
            key: Some("fixture-search-key".into()),
        };
        let provider: Box<dyn SearchProvider> = match kind {
            SearchProviderKind::SerpApi => Box::new(SerpApi(connection)),
            SearchProviderKind::Serper => Box::new(Serper(connection)),
            _ => Box::new(Perplexity(connection)),
        };
        let result = provider
            .search(
                &request(DomainFilter::Blocked(vec!["example.org".into()])),
                &context(),
            )
            .await
            .unwrap();
        assert!(result.results.is_empty());
        let wire = received.await.unwrap();
        if kind == SearchProviderKind::SerpApi {
            let target = wire.split_whitespace().nth(1).unwrap();
            let url = Url::parse(&format!("http://localhost{target}")).unwrap();
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "q" && value.ends_with(" -site:example.org"))
            );
        } else {
            let body: Value = serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            if kind == SearchProviderKind::Perplexity {
                assert_eq!(body["search_domain_filter"], json!(["-example.org"]));
            } else {
                assert!(body["q"].as_str().unwrap().ends_with(" -site:example.org"));
            }
        }
    }
}

#[tokio::test]
async fn insufficient_credits_report_a_typed_error_without_echoing_the_response() {
    let (url, received) = server(
        400,
        "application/json",
        r#"{"message":"Not enough credits PRIVATE-UPSTREAM-DETAIL"}"#,
    )
    .await;
    let result = http_json(url, Method::POST, HeaderMap::new(), None, &context()).await;
    let error = result.unwrap_err();
    assert!(matches!(error, SearchError::UsageLimit(400)));
    assert!(!error.to_string().contains("PRIVATE-UPSTREAM-DETAIL"));
    received.await.unwrap();
}

#[test]
fn request_limits_never_discard_domain_constraints() {
    let request = SearchRequest {
        query: "x".repeat(590),
        domains: DomainFilter::Allowed(vec!["example.com".into()]),
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    credentials::save_api_key(&root, "SEARCH_FIXTURE_KEY", "fixture").unwrap();
    let mut config = SearchConfig::for_provider(SearchProviderKind::Brave);
    config.api_key_env = Some("SEARCH_FIXTURE_KEY".into());
    assert!(matches!(
        prepare(&config, &request, Some(&root)),
        Err(SearchError::InvalidQuery(_))
    ));
    let mut body = json!({});
    add_domains(
        &mut body,
        &DomainFilter::Blocked(vec!["example.com".into()]),
        "include",
        "exclude",
    );
    assert_eq!(body, json!({"exclude":["example.com"]}));
    assert_eq!(
        DomainFilter::Allowed(vec!["a.com".into(), "b.com".into()]).operator_queries("a OR b", "-"),
        vec!["(a OR b) site:a.com", "(a OR b) site:b.com"]
    );
    assert_eq!(
        DomainFilter::Allowed(vec!["rust-lang.org".into()])
            .operator_queries("rust documentation", "-"),
        vec!["rust documentation site:rust-lang.org"]
    );
    assert_eq!(
        DomainFilter::Blocked(vec!["example.org".into()]).operator_queries("a | b", "-"),
        vec!["(a | b) -site:example.org"]
    );
}

#[tokio::test]
async fn duckduckgo_is_the_only_free_destination_and_reports_its_source() {
    let html = r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fdoc">A &amp; B</a><a class="result__snippet">A useful source</a></div>"#;
    let (endpoint, received) = server(200, "text/html", html).await;
    let provider = DuckDuckGoSearch { endpoint };
    let result = provider
        .search(&request(DomainFilter::Any), &context())
        .await
        .unwrap();
    let wire = received.await.unwrap();
    assert!(wire.starts_with("GET "));
    let user_agents: Vec<_> = wire
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("user-agent: ")
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        user_agents,
        vec![DUCKDUCKGO_USER_AGENT.to_ascii_lowercase()]
    );
    let target = wire.split_whitespace().nth(1).unwrap();
    let requested = Url::parse(&format!("http://localhost{target}")).unwrap();
    assert!(
        requested
            .query_pairs()
            .any(|(key, value)| key == "q" && value == "rust documentation")
    );
    assert_eq!(result.engine, Some("DuckDuckGo"));
    assert_eq!(result.results[0].url, "https://example.org/doc");
    assert_eq!(result.results[0].title, "A & B");
    let prepared = prepare(&SearchConfig::default(), &request(DomainFilter::Any), None).unwrap();
    assert_eq!(prepared.endpoints, vec![DUCKDUCKGO_ENDPOINT]);
    assert_eq!(prepared.kind.label(), "DuckDuckGo · Free");
}

#[test]
fn html_parsing_distinguishes_zero_results_challenges_and_layout_changes() {
    assert!(
        parse_search_page("<div class='no-results'>Nothing</div>")
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        parse_search_page("<form id='challenge-form'></form>"),
        Err(SearchError::Challenge)
    ));
    assert!(matches!(
        parse_search_page("<html>New incompatible layout</html>"),
        Err(SearchError::InvalidPage)
    ));
    assert_eq!(
        resolve_search_link("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fpage").unwrap(),
        "https://example.org/page"
    );
}

#[tokio::test]
async fn duckduckgo_challenges_are_errors_without_switching_to_another_engine() {
    let (endpoint, received) = server(202, "text/html", "<form id='challenge-form'></form>").await;
    let provider = DuckDuckGoSearch { endpoint };
    assert!(matches!(
        provider
            .search(&request(DomainFilter::Any), &context())
            .await,
        Err(SearchError::Challenge)
    ));
    received.await.unwrap();
}

#[tokio::test]
async fn duckduckgo_access_denials_do_not_ask_for_an_api_key() {
    let (endpoint, received) = server(403, "text/html", "Denied").await;
    let provider = DuckDuckGoSearch { endpoint };
    let Err(error) = provider
        .search(&request(DomainFilter::Any), &context())
        .await
    else {
        panic!("expected DuckDuckGo access denial")
    };
    assert!(matches!(error, SearchError::Http(403)));
    assert!(!error.to_string().contains("credentials"));
    received.await.unwrap();
}

#[tokio::test]
#[ignore = "explicit live reproduction of the user-reported Rust workspace query; no API key"]
async fn live_duckduckgo_rust_workspace_query_without_domain_filters() {
    let request = SearchRequest {
        query: "Rust Cargo workspaces official documentation doc.rust-lang.org cargo reference workspaces".into(),
        domains: DomainFilter::Any,
    };
    let prepared = prepare(&SearchConfig::default(), &request, None).unwrap();
    assert_eq!(prepared.endpoints, vec![DUCKDUCKGO_ENDPOINT]);
    let result = prepared
        .execute(&request, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.engine, Some("DuckDuckGo"));
    let first = result.results.first().expect("a Rust workspace source");
    assert!(
        first.url.contains("rust-lang.org"),
        "unexpected first result host"
    );
    assert!(
        first.title.to_ascii_lowercase().contains("workspace"),
        "unexpected first result topic"
    );
    println!(
        "LIVE exact user query: source=DuckDuckGo results={} first_source_is_official=true",
        result.results.len()
    );
}

#[tokio::test]
async fn failures_are_typed_and_do_not_include_upstream_bodies_or_keys() {
    for (status, expected) in [
        (403, "access denied"),
        (402, "usage or plan"),
        (429, "rate limited"),
        (200, "invalid JSON"),
    ] {
        let (url, received) =
            server(status, "application/json", "provider echoed fixture-secret").await;
        let result = http_json(url, Method::GET, HeaderMap::new(), None, &context()).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("fixture-secret"));
        received.await.unwrap();
    }
    let (url, task) = server(200, "application/json", "{}").await;
    let context = context();
    context.cancel.cancel();
    assert!(matches!(
        http_json(url, Method::GET, HeaderMap::new(), None, &context).await,
        Err(SearchError::Cancelled)
    ));
    task.abort();
}

#[test]
fn filtered_results_are_deduplicated_bounded_and_keep_unknown_dates_unknown() {
    let hits = (0..15)
        .map(|i| SearchHit {
            title: format!("Result {i}"),
            url: format!("https://docs.example.org/{i}"),
            snippet: None,
            published_at: None,
        })
        .collect();
    let result = response(
        &request(DomainFilter::Allowed(vec!["example.org".into()])),
        SearchProviderKind::Free,
        Some("DuckDuckGo"),
        hits,
        &context(),
    )
    .unwrap();
    assert!(result.truncated);
    assert_eq!(result.results.len(), DEFAULT_RESULT_LIMIT);
    assert!(result.results[0].published_at.is_none());
}

#[tokio::test]
#[ignore = "explicit real search provider verification; requires a selected provider and optionally a private API key"]
async fn live_selected_search_provider() {
    let name = std::env::var("TICKLOOP_WEB_SEARCH_LIVE_PROVIDER").expect("select a live provider");
    let kind = match name.as_str() {
        "tavily" => SearchProviderKind::Tavily,
        "exa" => SearchProviderKind::Exa,
        "brave" => SearchProviderKind::Brave,
        "serpapi" => SearchProviderKind::SerpApi,
        "serper" => SearchProviderKind::Serper,
        "perplexity" => SearchProviderKind::Perplexity,
        "free" => SearchProviderKind::Free,
        _ => panic!("unsupported live provider"),
    };
    let mut config = SearchConfig::for_provider(kind);
    if kind != SearchProviderKind::Free {
        config.api_key_env = Some("TICKLOOP_WEB_SEARCH_LIVE_API_KEY".into());
    }
    let request = request(DomainFilter::Allowed(vec!["rust-lang.org".into()]));
    let plan = prepare(&config, &request, None).unwrap();
    let result = plan.execute(&request, CancellationToken::new()).await;
    match result {
        Ok(result) => {
            assert!(
                result
                    .results
                    .iter()
                    .all(|hit| request.domains.admits(&Url::parse(&hit.url).unwrap()))
            );
            if kind != SearchProviderKind::Free {
                assert!(
                    !result.results.is_empty(),
                    "no official Rust sources returned"
                );
            }
            println!(
                "LIVE provider={} engine={:?} results={} domains_verified=true",
                kind.label(),
                result.engine,
                result.results.len()
            );
        }
        Err(error) if kind == SearchProviderKind::Free => {
            println!("LIVE provider=Free unavailable={error}")
        }
        Err(error) => panic!("live provider failed: {error}"),
    }
}

#[tokio::test]
async fn connection_tests_classify_real_adapter_responses_and_use_one_result() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    credentials::save_api_key(&root, "TEST_SEARCH_KEY", "private-fixture-key").unwrap();
    for (status, body, expected) in [
        (
            200,
            r#"{"results":[{"title":"Rust","url":"https://www.rust-lang.org/"}]}"#,
            SearchTestStatus::Available,
        ),
        (200, r#"{"results":[]}"#, SearchTestStatus::Unavailable),
        (401, "unauthorized", SearchTestStatus::Unavailable),
        (403, "forbidden", SearchTestStatus::Unavailable),
        (402, "payment", SearchTestStatus::Unavailable),
        (
            400,
            r#"{"message":"Not enough credits"}"#,
            SearchTestStatus::Unavailable,
        ),
        (429, "limited", SearchTestStatus::Unavailable),
        (503, "unavailable", SearchTestStatus::Unavailable),
        (200, "invalid JSON", SearchTestStatus::Unavailable),
    ] {
        let (url, received) = server(status, "application/json", body).await;
        let config = SearchConfig {
            provider: SearchProviderKind::Tavily,
            endpoint: url.to_string(),
            api_key_env: Some("TEST_SEARCH_KEY".into()),
        };
        let result = test_connection(&config, Some(&root), CancellationToken::new()).await;
        assert_eq!(result.status, expected, "HTTP {status}");
        assert!(!result.detail.contains("private-fixture-key"));
        let wire = received.await.unwrap();
        let body: Value = serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["max_results"], 1);
        assert_eq!(body["query"], "Rust programming language");
    }
    let config = SearchConfig {
        provider: SearchProviderKind::Tavily,
        endpoint: "http://127.0.0.1:1/search".into(),
        api_key_env: Some("TEST_SEARCH_KEY".into()),
    };
    assert_eq!(
        test_connection(&config, Some(&root), CancellationToken::new())
            .await
            .status,
        SearchTestStatus::Unavailable
    );
    credentials::remove_api_key(&root, "TEST_SEARCH_KEY").unwrap();
    assert_eq!(
        test_connection(&config, Some(&root), CancellationToken::new())
            .await
            .status,
        SearchTestStatus::Unavailable
    );
}

#[test]
fn duckduckgo_challenge_offers_manual_verification_and_provider_settings_without_switching() {
    let request = SearchRequest {
        query: "故宫 预约 & official".into(),
        domains: DomainFilter::Any,
    };
    let details =
        failure_details(SearchProviderKind::Free, &request, &SearchError::Challenge).unwrap();
    assert!(!details.reason.contains("/websearch"));
    assert_eq!(details.suggestions.len(), 2);
    assert!(matches!(
        &details.suggestions[0],
        WebSearchSuggestion::OpenVerification { .. }
    ));
    assert!(
        matches!(&details.suggestions[1], WebSearchSuggestion::ChangeProvider { command } if command == "/websearch")
    );
    let message = failure_message(SearchProviderKind::Free, &request, &SearchError::Challenge);
    assert!(message.contains("Open this search in your browser"));
    assert!(message.contains("same proxy/network"));
    assert!(message.contains("may not carry over"));
    assert!(message.contains("/websearch"));
    let url_text = message
        .split_whitespace()
        .find(|word| word.starts_with("https://"))
        .unwrap();
    let url = Url::parse(url_text).unwrap();
    assert_eq!(url.host_str(), Some("html.duckduckgo.com"));
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "q" && value == request.query)
    );
    let denied = failure_message(
        SearchProviderKind::Tavily,
        &request,
        &SearchError::Authentication(401),
    );
    assert!(!denied.contains("Open this search"));
}

#[tokio::test]
#[ignore = "explicit live DuckDuckGo reproduction of the user-reported Palace Museum query"]
async fn live_duckduckgo_palace_museum_query() {
    let request = SearchRequest {
        query: "故宫博物院 www.dpm.org.cn 参观 预约 购票 官方".into(),
        domains: DomainFilter::Any,
    };
    let plan = prepare(&SearchConfig::default(), &request, None).unwrap();
    match plan.execute(&request, CancellationToken::new()).await {
        Ok(response) => println!(
            "LIVE Palace Museum query: results={}",
            response.results.len()
        ),
        Err(error) => println!("LIVE Palace Museum query: unavailable={error}"),
    }
}
