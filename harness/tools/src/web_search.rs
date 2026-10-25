//! Provider-independent search requests. Credentials and destinations are bound before approval.

use std::{
    collections::HashSet,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderValue},
};
use scraper::{Html, Selector};
use serde_json::{Value, json};
use tickloop_harness_base::{
    credentials,
    network::{HttpPolicy, build_http_client},
};
use tickloop_harness_protocol::{
    SearchConfig, SearchProviderKind, SearchTestResult, SearchTestStatus, WebSearchFailure,
    WebSearchSuggestion,
};
use tokio_util::sync::CancellationToken;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 4096;
const PERPLEXITY_MAX_DOMAIN_BYTES: usize = 253;
pub const DEFAULT_RESULT_LIMIT: usize = 10;
pub const DISABLED_MESSAGE: &str = "Web search is disabled for this request. The user can enable it with /websearch. This call was not executed.";
const DUCKDUCKGO_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const DUCKDUCKGO_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainFilter {
    Any,
    Allowed(Vec<String>),
    Blocked(Vec<String>),
}

impl DomainFilter {
    pub fn admits(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or_default().trim_end_matches('.');
        let listed = |domains: &[String]| {
            domains.iter().any(|domain| {
                host == domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
        };
        match self {
            Self::Any => true,
            Self::Allowed(domains) => listed(domains),
            Self::Blocked(domains) => !listed(domains),
        }
    }

    fn operator_queries(&self, query: &str, exclusion_prefix: &str) -> Vec<String> {
        let constrained_query = if query
            .split_whitespace()
            .any(|word| word.eq_ignore_ascii_case("OR") || word == "|")
        {
            format!("({query})")
        } else {
            query.to_owned()
        };
        match self {
            Self::Any => vec![query.to_owned()],
            // Separate requests avoid relying on engine-specific OR precedence for a whitelist.
            Self::Allowed(domains) => domains
                .iter()
                .map(|domain| format!("{constrained_query} site:{domain}"))
                .collect(),
            Self::Blocked(domains) => vec![format!(
                "{constrained_query} {}",
                domains
                    .iter()
                    .map(|domain| format!("{exclusion_prefix}site:{domain}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )],
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub domains: DomainFilter,
}

pub struct SearchContext {
    pub cancel: CancellationToken,
    pub deadline: tokio::time::Instant,
    pub max_results: usize,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    pub published_at: Option<String>,
}

pub struct SearchResponse {
    pub query: String,
    pub provider: SearchProviderKind,
    pub engine: Option<&'static str>,
    pub results: Vec<SearchHit>,
    pub searched_at: SystemTime,
    pub truncated: bool,
}

pub struct SearchProviderDescriptor {
    pub kind: SearchProviderKind,
    pub name: &'static str,
    pub credential_env: Option<&'static str>,
}

pub fn descriptor(kind: SearchProviderKind) -> SearchProviderDescriptor {
    SearchProviderDescriptor {
        kind,
        name: kind.label(),
        credential_env: kind.credential_env(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("invalid search query: {0}")]
    InvalidQuery(&'static str),
    #[error("search credentials are missing; configure the provider in /websearch")]
    MissingCredential,
    #[error("saved search credentials are unavailable or unsafe")]
    Credential,
    #[error("service access denied (HTTP {0}); check the selected provider credentials")]
    Authentication(u16),
    #[error(
        "search provider usage or plan limit (HTTP {0}); check the account balance or subscription"
    )]
    UsageLimit(u16),
    #[error("service rate limited the request (HTTP 429)")]
    RateLimited { retry_after: Option<Duration> },
    #[error("search engine requires human verification; no search results were returned")]
    Challenge,
    #[error("connection or response transfer failed")]
    Network,
    #[error("request timed out")]
    Timeout,
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error("search response has invalid JSON or missing result fields")]
    InvalidResponse,
    #[error("search page format is unsupported; no search results could be parsed")]
    InvalidPage,
    #[error("response exceeds 10 MB; no content was returned")]
    TooLarge,
    #[error("search service redirected; configure the final endpoint in settings")]
    Redirect,
    #[error("search service returned HTTP {0}")]
    Http(u16),
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn descriptor(&self) -> SearchProviderDescriptor;
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError>;
}

pub struct PreparedSearch {
    provider: Arc<dyn SearchProvider>,
    pub endpoints: Vec<String>,
    pub kind: SearchProviderKind,
}

impl std::fmt::Debug for PreparedSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSearch")
            .field("provider", &self.kind)
            .finish_non_exhaustive()
    }
}

impl PreparedSearch {
    pub async fn execute(
        &self,
        request: &SearchRequest,
        cancel: CancellationToken,
    ) -> Result<SearchResponse, SearchError> {
        self.execute_with_limit(request, cancel, DEFAULT_RESULT_LIMIT)
            .await
    }

    async fn execute_with_limit(
        &self,
        request: &SearchRequest,
        cancel: CancellationToken,
        max_results: usize,
    ) -> Result<SearchResponse, SearchError> {
        let context = SearchContext {
            cancel,
            deadline: tokio::time::Instant::now() + SEARCH_TIMEOUT,
            max_results,
        };
        tokio::select! {
            biased;
            _ = context.cancel.cancelled() => Err(SearchError::Cancelled),
            result = tokio::time::timeout_at(context.deadline, self.provider.search(request, &context)) => result.map_err(|_| SearchError::Timeout)?,
        }
    }
}

/// Runs one ordinary search through the selected adapter without changing configuration.
pub async fn test_connection(
    config: &SearchConfig,
    credential_root: Option<&Path>,
    cancel: CancellationToken,
) -> SearchTestResult {
    let request = SearchRequest {
        query: "Rust programming language".into(),
        domains: DomainFilter::Any,
    };
    let result = match prepare(config, &request, credential_root) {
        Ok(prepared) => prepared.execute_with_limit(&request, cancel, 1).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(response) if !response.results.is_empty() => SearchTestResult {
            status: SearchTestStatus::Available,
            detail: "Test search returned a valid result. This is the latest test, not an ongoing availability check.".into(),
        },
        Ok(_) => SearchTestResult {
            status: SearchTestStatus::Unavailable,
            detail: "The service responded without results; search access could not be verified.".into(),
        },
        Err(error) => SearchTestResult {
            status: SearchTestStatus::Unavailable,
            detail: failure_message(config.provider, &request, &error),
        },
    }
}

pub fn failure_details(
    kind: SearchProviderKind,
    request: &SearchRequest,
    error: &SearchError,
) -> Option<WebSearchFailure> {
    if kind != SearchProviderKind::Free || !matches!(error, SearchError::Challenge) {
        return None;
    }
    let mut verification_url = Url::parse(DUCKDUCKGO_ENDPOINT).expect("valid engine");
    verification_url
        .query_pairs_mut()
        .append_pair("q", &request.query);
    Some(WebSearchFailure {
        reason: format!("{}: {error}", kind.label()),
        suggestions: vec![
            WebSearchSuggestion::OpenVerification {
                url: verification_url.to_string(),
                note: "Use the same proxy/network as Tickloop. Browser verification may not carry over to HTTP search.".into(),
            },
            WebSearchSuggestion::ChangeProvider { command: "/websearch".into() },
        ],
    })
}

pub fn failure_message(
    kind: SearchProviderKind,
    request: &SearchRequest,
    error: &SearchError,
) -> String {
    let Some(failure) = failure_details(kind, request, error) else {
        return format!("{}: {error}", kind.label());
    };
    let mut message = failure.reason;
    message.push_str("\n\nNext steps:");
    for suggestion in failure.suggestions {
        match suggestion {
            WebSearchSuggestion::OpenVerification { url, note } => {
                message.push_str(&format!("\nOpen this search in your browser to complete verification, then retry: {url}\n{note}"));
            }
            WebSearchSuggestion::ChangeProvider { command } => {
                message.push_str(&format!(
                    "\nOr use {command} to choose another search provider."
                ));
            }
        }
    }
    message
}

pub fn validate_config(config: &SearchConfig) -> Result<(), String> {
    if config.provider == SearchProviderKind::Free {
        if !config.endpoint.is_empty() || config.api_key_env.is_some() {
            return Err("DuckDuckGo uses its fixed endpoint and no API key".into());
        }
        return Ok(());
    }
    endpoint(config).map_err(|_| "search endpoint must use HTTPS without credentials, query or fragment (loopback HTTP is allowed)".to_owned())?;
    if let Some(name) = &config.api_key_env {
        let mut chars = name.chars();
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("search credentials must name an environment variable".into());
        }
    }
    Ok(())
}

fn endpoint(config: &SearchConfig) -> Result<Url, SearchError> {
    let address = if config.endpoint.is_empty() {
        config.provider.default_endpoint()
    } else {
        &config.endpoint
    };
    let url = Url::parse(address).map_err(|_| SearchError::InvalidQuery("invalid endpoint"))?;
    if !(url.scheme() == "https"
        || url.scheme() == "http" && tickloop_harness_base::network::is_loopback(&url))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SearchError::InvalidQuery("invalid endpoint"));
    }
    Ok(url)
}

pub fn prepare(
    config: &SearchConfig,
    request: &SearchRequest,
    credential_root: Option<&Path>,
) -> Result<Arc<PreparedSearch>, SearchError> {
    validate_config(config)
        .map_err(|_| SearchError::InvalidQuery("invalid provider configuration"))?;
    if request.query.len() > MAX_QUERY_BYTES {
        return Err(SearchError::InvalidQuery("query is too long"));
    }
    let kind = config.provider;
    let (provider, endpoints): (Arc<dyn SearchProvider>, Vec<String>) = if kind
        == SearchProviderKind::Free
    {
        let queries = request.domains.operator_queries(&request.query, "-");
        validate_queries(&queries, MAX_QUERY_BYTES, None)?;
        (
            Arc::new(DuckDuckGoSearch {
                endpoint: Url::parse(DUCKDUCKGO_ENDPOINT).expect("valid engine"),
            }),
            vec![DUCKDUCKGO_ENDPOINT.to_owned()],
        )
    } else {
        let url = endpoint(config)?;
        let name = config.api_key_env.as_deref().or(kind.credential_env());
        let key = name
            .map(|name| {
                let saved = credential_root
                    .map(|root| credentials::load_api_key(root, name))
                    .transpose()
                    .map_err(|_| SearchError::Credential)?
                    .flatten();
                saved
                    .or_else(|| std::env::var(name).ok())
                    .filter(|key| !key.trim().is_empty())
                    .ok_or(SearchError::MissingCredential)
            })
            .transpose()?;
        let connection = Connection {
            url: url.clone(),
            key,
        };
        let provider: Arc<dyn SearchProvider> = match kind {
            SearchProviderKind::Brave => {
                validate_queries(
                    &request.domains.operator_queries(&request.query, "NOT "),
                    600,
                    Some(75),
                )?;
                Arc::new(Brave(connection))
            }
            SearchProviderKind::Tavily => Arc::new(Tavily(connection)),
            SearchProviderKind::Exa => Arc::new(Exa(connection)),
            SearchProviderKind::SerpApi => {
                validate_queries(
                    &request.domains.operator_queries(&request.query, "-"),
                    MAX_QUERY_BYTES,
                    None,
                )?;
                Arc::new(SerpApi(connection))
            }
            SearchProviderKind::Serper => {
                validate_queries(
                    &request.domains.operator_queries(&request.query, "-"),
                    MAX_QUERY_BYTES,
                    None,
                )?;
                Arc::new(Serper(connection))
            }
            SearchProviderKind::Perplexity => {
                let (domains, prefix_length): (&[String], usize) = match &request.domains {
                    DomainFilter::Any => (&[], 0),
                    DomainFilter::Allowed(domains) => (domains, 0),
                    DomainFilter::Blocked(domains) => (domains, 1),
                };
                if domains
                    .iter()
                    .any(|domain| domain.len() + prefix_length > PERPLEXITY_MAX_DOMAIN_BYTES)
                {
                    return Err(SearchError::InvalidQuery(
                        "Perplexity domains including exclusion prefixes must not exceed 253 bytes",
                    ));
                }
                Arc::new(Perplexity(connection))
            }
            SearchProviderKind::Searxng => {
                validate_queries(
                    &request.domains.operator_queries(&request.query, "-"),
                    MAX_QUERY_BYTES,
                    None,
                )?;
                Arc::new(Searxng(connection))
            }
            SearchProviderKind::Free => unreachable!(),
        };
        (provider, vec![url.to_string()])
    };
    Ok(Arc::new(PreparedSearch {
        provider,
        endpoints,
        kind,
    }))
}

fn validate_queries(
    queries: &[String],
    max_chars: usize,
    max_words: Option<usize>,
) -> Result<(), SearchError> {
    if queries.len() > DEFAULT_RESULT_LIMIT {
        return Err(SearchError::InvalidQuery(
            "at most 10 domains can be searched in one request",
        ));
    }
    if queries.iter().any(|query| {
        query.chars().count() > max_chars
            || max_words.is_some_and(|limit| query.split_whitespace().count() > limit)
    }) {
        return Err(SearchError::InvalidQuery(
            "query including domain filters exceeds this provider's limit; shorten it or use fewer domains",
        ));
    }
    Ok(())
}

struct Connection {
    url: Url,
    key: Option<String>,
}

impl Connection {
    fn headers(&self, name: &'static str, bearer: bool) -> Result<HeaderMap, SearchError> {
        let mut headers = HeaderMap::new();
        if let Some(key) = &self.key {
            let value = if bearer {
                format!("Bearer {key}")
            } else {
                key.clone()
            };
            let mut value = HeaderValue::from_str(&value).map_err(|_| SearchError::Credential)?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

async fn http(
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<Value>,
    context: &SearchContext,
) -> Result<(String, Vec<u8>), SearchError> {
    let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
    policy.timeout = Some(
        context
            .deadline
            .saturating_duration_since(tokio::time::Instant::now()),
    );
    let (client, _) = build_http_client(&url, policy).map_err(|_| SearchError::Network)?;
    let mut headers = headers;
    headers
        .entry(reqwest::header::USER_AGENT)
        .or_insert_with(|| {
            HeaderValue::from_static(concat!("Tickloop-Agent/", env!("CARGO_PKG_VERSION")))
        });
    let mut request = client.request(method, url).headers(headers);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let work = async {
        let mut response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                SearchError::Timeout
            } else {
                SearchError::Network
            }
        })?;
        let status = response.status();
        if status.is_redirection() {
            return Err(SearchError::Redirect);
        }
        match status.as_u16() {
            401 | 403 => return Err(SearchError::Authentication(status.as_u16())),
            402 => return Err(SearchError::UsageLimit(402)),
            429 => {
                return Err(SearchError::RateLimited {
                    retry_after: response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(Duration::from_secs),
                });
            }
            _ => {}
        }
        if !status.is_success() && status != reqwest::StatusCode::BAD_REQUEST {
            return Err(SearchError::Http(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(SearchError::TooLarge);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| SearchError::Network)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(SearchError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if status == reqwest::StatusCode::BAD_REQUEST {
            let error: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let message = error
                .get("message")
                .or_else(|| error.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if [
                "not enough credits",
                "insufficient credits",
                "quota exceeded",
                "run out of searches",
            ]
            .iter()
            .any(|marker| message.contains(marker))
            {
                return Err(SearchError::UsageLimit(status.as_u16()));
            }
            return Err(SearchError::Http(status.as_u16()));
        }
        Ok((content_type, bytes))
    };
    tokio::select! { biased; _ = context.cancel.cancelled() => Err(SearchError::Cancelled), result = work => result }
}

async fn http_json(
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<Value>,
    context: &SearchContext,
) -> Result<Value, SearchError> {
    let (content_type, bytes) = http(url, method, headers, body, context).await?;
    if !content_type.starts_with("application/json") {
        return Err(SearchError::InvalidResponse);
    }
    serde_json::from_slice(&bytes).map_err(|_| SearchError::InvalidResponse)
}

fn response(
    request: &SearchRequest,
    kind: SearchProviderKind,
    engine: Option<&'static str>,
    hits: Vec<SearchHit>,
    context: &SearchContext,
) -> Result<SearchResponse, SearchError> {
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for hit in hits {
        let url = Url::parse(&hit.url).map_err(|_| SearchError::InvalidResponse)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(SearchError::InvalidResponse);
        }
        if request.domains.admits(&url) && seen.insert(url.to_string()) {
            results.push(SearchHit {
                url: url.to_string(),
                ..hit
            });
        }
    }
    let truncated = results.len() > context.max_results;
    results.truncate(context.max_results);
    Ok(SearchResponse {
        query: request.query.clone(),
        provider: kind,
        engine,
        results,
        searched_at: SystemTime::now(),
        truncated,
    })
}

struct Brave(Connection);
struct Tavily(Connection);
struct Exa(Connection);
struct SerpApi(Connection);
struct Serper(Connection);
struct Perplexity(Connection);
struct Searxng(Connection);

#[async_trait]
impl SearchProvider for Brave {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Brave)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut hits = Vec::new();
        for query in request.domains.operator_queries(&request.query, "NOT ") {
            let mut url = self.0.url.clone();
            url.query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("count", &context.max_results.min(20).to_string())
                .append_pair("text_decorations", "false");
            let value = http_json(
                url,
                Method::GET,
                self.0.headers("x-subscription-token", false)?,
                None,
                context,
            )
            .await?;
            let entries = value.pointer("/web/results").and_then(Value::as_array);
            if let Some(entries) = entries {
                hits.extend(json_hits(entries, "url", "description", "page_age")?);
            } else if value.get("query").is_none() {
                return Err(SearchError::InvalidResponse);
            }
        }
        response(request, SearchProviderKind::Brave, None, hits, context)
    }
}

#[async_trait]
impl SearchProvider for Tavily {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Tavily)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut body = json!({"query": request.query, "max_results": context.max_results.min(20), "search_depth": "basic", "include_answer": false});
        add_domains(
            &mut body,
            &request.domains,
            "include_domains",
            "exclude_domains",
        );
        if matches!(request.domains, DomainFilter::Allowed(_)) {
            body["include_domains_mode"] = json!("restrict");
        }
        let value = http_json(
            self.0.url.clone(),
            Method::POST,
            self.0.headers("authorization", true)?,
            Some(body),
            context,
        )
        .await?;
        let entries = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or(SearchError::InvalidResponse)?;
        response(
            request,
            SearchProviderKind::Tavily,
            None,
            json_hits(entries, "url", "content", "published_date")?,
            context,
        )
    }
}

#[async_trait]
impl SearchProvider for Exa {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Exa)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut body = json!({"query": request.query, "numResults": context.max_results, "type": "auto", "contents": {"highlights": true}});
        add_domains(
            &mut body,
            &request.domains,
            "includeDomains",
            "excludeDomains",
        );
        let value = http_json(
            self.0.url.clone(),
            Method::POST,
            self.0.headers("x-api-key", false)?,
            Some(body),
            context,
        )
        .await?;
        let entries = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or(SearchError::InvalidResponse)?;
        let mut hits = json_hits(entries, "url", "text", "publishedDate")?;
        for (hit, entry) in hits.iter_mut().zip(entries) {
            if let Some(highlights) = entry.get("highlights").and_then(Value::as_array) {
                let snippets = highlights
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                if !snippets.is_empty() {
                    hit.snippet = Some(snippets.join("\n"));
                }
            }
        }
        response(request, SearchProviderKind::Exa, None, hits, context)
    }
}

#[async_trait]
impl SearchProvider for SerpApi {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::SerpApi)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut hits = Vec::new();
        for query in request.domains.operator_queries(&request.query, "-") {
            let mut url = self.0.url.clone();
            url.query_pairs_mut()
                .append_pair("engine", "google")
                .append_pair("q", &query)
                .append_pair("num", &context.max_results.to_string())
                .append_pair(
                    "api_key",
                    self.0
                        .key
                        .as_deref()
                        .ok_or(SearchError::MissingCredential)?,
                );
            let value = http_json(url, Method::GET, HeaderMap::new(), None, context).await?;
            if let Some(entries) = value.get("organic_results").and_then(Value::as_array) {
                hits.extend(json_hits(entries, "link", "snippet", "date")?);
            } else if value
                .pointer("/search_information/organic_results_state")
                .and_then(Value::as_str)
                != Some("Fully empty")
            {
                return Err(SearchError::InvalidResponse);
            }
        }
        response(request, SearchProviderKind::SerpApi, None, hits, context)
    }
}

#[async_trait]
impl SearchProvider for Serper {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Serper)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut hits = Vec::new();
        for query in request.domains.operator_queries(&request.query, "-") {
            let value = http_json(
                self.0.url.clone(),
                Method::POST,
                self.0.headers("x-api-key", false)?,
                Some(json!({"q": query, "num": context.max_results})),
                context,
            )
            .await?;
            let entries = value
                .get("organic")
                .and_then(Value::as_array)
                .ok_or(SearchError::InvalidResponse)?;
            hits.extend(json_hits(entries, "link", "snippet", "date")?);
        }
        response(request, SearchProviderKind::Serper, None, hits, context)
    }
}

#[async_trait]
impl SearchProvider for Perplexity {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Perplexity)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut body = json!({"query": request.query, "max_results": context.max_results, "search_type": "web"});
        match &request.domains {
            DomainFilter::Any => {}
            DomainFilter::Allowed(domains) => body["search_domain_filter"] = json!(domains),
            DomainFilter::Blocked(domains) => {
                body["search_domain_filter"] = json!(
                    domains
                        .iter()
                        .map(|domain| format!("-{domain}"))
                        .collect::<Vec<_>>()
                )
            }
        }
        let value = http_json(
            self.0.url.clone(),
            Method::POST,
            self.0.headers("authorization", true)?,
            Some(body),
            context,
        )
        .await?;
        let entries = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or(SearchError::InvalidResponse)?;
        response(
            request,
            SearchProviderKind::Perplexity,
            None,
            json_hits(entries, "url", "snippet", "date")?,
            context,
        )
    }
}

#[async_trait]
impl SearchProvider for Searxng {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Searxng)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut hits = Vec::new();
        for query in request.domains.operator_queries(&request.query, "-") {
            let mut url = self.0.url.clone();
            url.query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("format", "json");
            let value = http_json(
                url,
                Method::GET,
                self.0.headers("x-api-key", false)?,
                None,
                context,
            )
            .await?;
            let entries = value
                .get("results")
                .and_then(Value::as_array)
                .ok_or(SearchError::InvalidResponse)?;
            hits.extend(json_hits(entries, "url", "content", "publishedDate")?);
        }
        response(request, SearchProviderKind::Searxng, None, hits, context)
    }
}

fn add_domains(body: &mut Value, domains: &DomainFilter, allowed: &str, blocked: &str) {
    match domains {
        DomainFilter::Any => {}
        DomainFilter::Allowed(domains) => body[allowed] = json!(domains),
        DomainFilter::Blocked(domains) => body[blocked] = json!(domains),
    }
}

fn json_hits(
    entries: &[Value],
    url: &str,
    snippet: &str,
    published: &str,
) -> Result<Vec<SearchHit>, SearchError> {
    entries
        .iter()
        .map(|entry| {
            Ok(SearchHit {
                title: entry
                    .get("title")
                    .and_then(Value::as_str)
                    .ok_or(SearchError::InvalidResponse)?
                    .to_owned(),
                url: entry
                    .get(url)
                    .and_then(Value::as_str)
                    .ok_or(SearchError::InvalidResponse)?
                    .to_owned(),
                snippet: entry
                    .get(snippet)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                published_at: entry
                    .get(published)
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

struct DuckDuckGoSearch {
    endpoint: Url,
}

#[async_trait]
impl SearchProvider for DuckDuckGoSearch {
    fn descriptor(&self) -> SearchProviderDescriptor {
        descriptor(SearchProviderKind::Free)
    }
    async fn search(
        &self,
        request: &SearchRequest,
        context: &SearchContext,
    ) -> Result<SearchResponse, SearchError> {
        let mut hits = Vec::new();
        for query in request.domains.operator_queries(&request.query, "-") {
            let mut url = self.endpoint.clone();
            url.query_pairs_mut().append_pair("q", &query);
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::USER_AGENT,
                HeaderValue::from_static(DUCKDUCKGO_USER_AGENT),
            );
            let (content_type, bytes) = http(url, Method::GET, headers, None, context)
                .await
                .map_err(|error| match error {
                    SearchError::Authentication(status) => SearchError::Http(status),
                    other => other,
                })?;
            if !content_type.starts_with("text/html") {
                return Err(SearchError::InvalidPage);
            }
            let text = String::from_utf8(bytes).map_err(|_| SearchError::InvalidPage)?;
            hits.extend(parse_search_page(&text)?);
        }
        response(
            request,
            SearchProviderKind::Free,
            Some("DuckDuckGo"),
            hits,
            context,
        )
    }
}

fn parse_search_page(text: &str) -> Result<Vec<SearchHit>, SearchError> {
    let document = Html::parse_document(text);
    let selector = |s: &str| Selector::parse(s).expect("static search selector");
    let lower = text.to_ascii_lowercase();
    let link = selector("a.result__a");
    let snippet = selector(".result__snippet");
    let mut hits = Vec::new();
    for row in document.select(&selector(".result")) {
        let Some(anchor) = row.select(&link).next() else {
            continue;
        };
        let Some(address) = anchor.value().attr("href") else {
            continue;
        };
        let title = anchor
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let url = resolve_search_link(address)?;
        if title.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title,
            url,
            snippet: row
                .select(&snippet)
                .next()
                .map(|s| s.text().collect::<Vec<_>>().join(" ")),
            published_at: None,
        });
    }
    if hits.is_empty()
        && [
            "challenge-form",
            "anomaly.js",
            "g-recaptcha",
            "are you a human",
            "verify you are human",
            "b_captcha",
            "cf-chl-",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return Err(SearchError::Challenge);
    }
    if hits.is_empty()
        && document
            .select(&selector(".no-results, .result--no-result"))
            .next()
            .is_none()
    {
        return Err(SearchError::InvalidPage);
    }
    Ok(hits)
}

fn resolve_search_link(address: &str) -> Result<String, SearchError> {
    let address = if address.starts_with("//") {
        format!("https:{address}")
    } else {
        address.to_owned()
    };
    let url = Url::parse(DUCKDUCKGO_ENDPOINT)
        .expect("valid engine")
        .join(&address)
        .map_err(|_| SearchError::InvalidPage)?;
    if url
        .host_str()
        .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
        && let Some((_, target)) = url.query_pairs().find(|(key, _)| key == "uddg")
    {
        return Ok(target.into_owned());
    }
    Ok(url.to_string())
}

#[cfg(test)]
#[path = "web_search_tests.rs"]
mod tests;
