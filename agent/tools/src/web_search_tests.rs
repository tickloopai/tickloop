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
    ] {
        let body = match kind {
            SearchProviderKind::Brave => {
                json!({"query": {}, "web": {"results": [{"title":"Rust", "url":"https://doc.rust-lang.org/", "description":"Reference"}, {"title":"Wrong host", "url":"https://example.org/"}]}})
            }
            SearchProviderKind::Tavily => {
                json!({"results":[{"title":"Rust", "url":"https://doc.rust-lang.org/", "content":"Reference"}, {"title":"Wrong host", "url":"https://example.org/"}]})
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
            _ => unreachable!(),
        }
    }
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
}

#[tokio::test]
async fn free_mode_falls_back_on_challenges_and_reports_the_actual_engine() {
    let (bing, first) = server(200, "text/html", "<form id=\"challenge-form\"></form>").await;
    let html = r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fdoc">A &amp; B</a><a class="result__snippet">A useful source</a></div>"#;
    let (duckduckgo, second) = server(200, "text/html", html).await;
    let provider = FreeSearch { bing, duckduckgo };
    let result = provider
        .search(&request(DomainFilter::Any), &context())
        .await
        .unwrap();
    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(result.engine, Some("DuckDuckGo"));
    assert_eq!(result.results[0].url, "https://example.org/doc");
    assert_eq!(result.results[0].title, "A & B");
    let prepared = prepare(&SearchConfig::default(), &request(DomainFilter::Any), None).unwrap();
    assert_eq!(prepared.endpoints, vec![BING_ENDPOINT, DUCKDUCKGO_ENDPOINT]);
}

#[test]
fn html_parsing_distinguishes_zero_results_challenges_and_layout_changes() {
    assert!(
        parse_search_page(FreeEngine::Bing, "<div class='b_no'>No results</div>")
            .unwrap()
            .is_empty()
    );
    assert!(
        parse_search_page(
            FreeEngine::DuckDuckGo,
            "<div class='no-results'>Nothing</div>"
        )
        .unwrap()
        .is_empty()
    );
    assert!(matches!(
        parse_search_page(FreeEngine::DuckDuckGo, "<form id='challenge-form'></form>"),
        Err(SearchError::Challenge)
    ));
    assert!(matches!(
        parse_search_page(FreeEngine::Bing, "<html>New incompatible layout</html>"),
        Err(SearchError::InvalidPage)
    ));
    use base64::Engine;
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("https://example.org/page");
    let html = format!(
        "<li class='b_algo'><h2><a href='https://www.bing.com/ck/a?u=a1{encoded}'>Source</a></h2><div class='b_caption'><p>Summary</p></div></li>"
    );
    assert_eq!(
        parse_search_page(FreeEngine::Bing, &html).unwrap()[0].url,
        "https://example.org/page"
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
        Some("Bing"),
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
