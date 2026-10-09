//! Claude Code's `WebFetch` (`WebFetchTool.ts`, `utils.ts`, `prompt.ts`) and `WebSearch`
//! (`WebSearchTool.ts`, `prompt.ts`), reading the web through the Chat's HTTP client and the
//! configured search service.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tickloop_agent_base::network::{HttpPolicy, NetworkDiagnostic, build_http_client};
use tokio_util::sync::CancellationToken;

use super::{
    Operation, ToolDefinition, ToolError, read::semantic_integer, shell::BASH_TOOL_NAME,
    utf16_length,
};
use tickloop_agent_protocol::{SearchConfig, ToolResult};

pub const WEB_FETCH_TOOL_NAME: &str = "WebFetch";
pub const WEB_SEARCH_TOOL_NAME: &str = "WebSearch";

/// Claude Code `MAX_URL_LENGTH`, counted in UTF-16 code units as JavaScript does.
const MAX_URL_LENGTH: usize = 2000;
/// Claude Code `MAX_HTTP_CONTENT_LENGTH`.
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// Claude Code `FETCH_TIMEOUT_MS`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Claude Code `MAX_MARKDOWN_LENGTH`, counted in UTF-16 code units as JavaScript does.
const MAX_MARKDOWN_LENGTH: usize = 100_000;
const MARKDOWN_TRUNCATION_NOTICE: &str = "\n\n[Content truncated due to length...]";
/// Claude Code `CACHE_TTL_MS` and `MAX_CACHE_SIZE_BYTES`.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_CACHE_BYTES: usize = 50 * 1024 * 1024;
/// The guidelines of Claude Code `makeSecondaryModelPrompt` for a domain that is not preapproved.
const SECONDARY_MODEL_GUIDELINES: &str = "Provide a concise response based only on the content above. In your response:
 - Enforce a strict 125-character maximum for quotes from any source document. Open Source Software is ok as long as we respect the license.
 - Use quotation marks for exact language from articles; any language outside of the quotation should never be word-for-word the same.
 - You are not a lawyer and never comment on the legality of your own prompts and responses.
 - Never produce or reproduce exact song lyrics.";
const MIN_QUERY_LENGTH: usize = 2;
const MAX_QUERY_BYTES: usize = 4096;
const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;
const NO_LINKS_FOUND: &str = "No links found.";
const SOURCES_REMINDER: &str = "REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.";
const BOTH_DOMAIN_LISTS_ERROR: &str =
    "Error: Cannot specify both allowed_domains and blocked_domains in the same request";

/// Answers a `WebFetch` prompt about a fetched page in one request outside the conversation, as
/// Claude Code's `applyPromptToMarkdown` asks Haiku.
#[async_trait]
pub trait SecondaryModel: Send + Sync {
    async fn respond(&self, prompt: String, cancel: CancellationToken)
    -> Result<String, ToolError>;
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FetchInput {
    url: String,
    prompt: String,
}

#[derive(Clone, Debug)]
pub(super) struct SearchInput {
    query: String,
    domains: DomainFilter,
    limit: usize,
    pub(super) config: Option<SearchConfig>,
}

/// Which result hosts a search keeps; each domain also covers its subdomains.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DomainFilter {
    Any,
    Allowed(Vec<String>),
    Blocked(Vec<String>),
}

impl DomainFilter {
    fn admits(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or_default().trim_end_matches('.');
        let is_listed = |domains: &[String]| {
            domains.iter().any(|domain| {
                host == domain
                    || host
                        .strip_suffix(domain.as_str())
                        .is_some_and(|subdomain| subdomain.ends_with('.'))
            })
        };
        match self {
            Self::Any => true,
            Self::Allowed(domains) => is_listed(domains),
            Self::Blocked(domains) => !is_listed(domains),
        }
    }
}

/// Claude Code's `URL_CACHE`: the Markdown of pages fetched in the last 15 minutes by requested
/// URL, dropping the least recently used pages once they hold more than 50 MB.
#[derive(Clone, Default)]
pub(super) struct FetchCache(Arc<Mutex<PageCache>>);

impl FetchCache {
    fn get(&self, url: &str) -> Option<Arc<str>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(url, Instant::now())
    }

    fn insert(&self, url: &str, markdown: Arc<str>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url, markdown, Instant::now());
    }
}

struct PageCache {
    pages: HashMap<String, CachedPage>,
    total_bytes: usize,
    capacity_bytes: usize,
    last_use: u64,
}

struct CachedPage {
    markdown: Arc<str>,
    stored_at: Instant,
    last_use: u64,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::with_capacity(MAX_CACHE_BYTES)
    }
}

impl PageCache {
    fn with_capacity(capacity_bytes: usize) -> Self {
        Self {
            pages: HashMap::new(),
            total_bytes: 0,
            capacity_bytes,
            last_use: 0,
        }
    }

    fn get(&mut self, url: &str, now: Instant) -> Option<Arc<str>> {
        if self
            .pages
            .get(url)
            .is_some_and(|page| now.saturating_duration_since(page.stored_at) >= CACHE_TTL)
        {
            self.remove(url);
            return None;
        }
        self.last_use += 1;
        let page = self.pages.get_mut(url)?;
        page.last_use = self.last_use;
        Some(page.markdown.clone())
    }

    fn insert(&mut self, url: &str, markdown: Arc<str>, now: Instant) {
        self.remove(url);
        self.pages
            .retain(|_, page| now.saturating_duration_since(page.stored_at) < CACHE_TTL);
        self.total_bytes = self
            .pages
            .values()
            .map(|page| page_size(&page.markdown))
            .sum();
        let size = page_size(&markdown);
        if size > self.capacity_bytes {
            return;
        }
        while self.total_bytes + size > self.capacity_bytes {
            let Some(least_recent) = self
                .pages
                .iter()
                .min_by_key(|(_, page)| page.last_use)
                .map(|(url, _)| url.clone())
            else {
                break;
            };
            self.remove(&least_recent);
        }
        self.last_use += 1;
        self.total_bytes += size;
        self.pages.insert(
            url.to_owned(),
            CachedPage {
                markdown,
                stored_at: now,
                last_use: self.last_use,
            },
        );
    }

    fn remove(&mut self, url: &str) {
        if let Some(page) = self.pages.remove(url) {
            self.total_bytes -= page_size(&page.markdown);
        }
    }
}

/// Claude Code sizes cache entries by their UTF-8 bytes, and never below one.
fn page_size(markdown: &str) -> usize {
    markdown.len().max(1)
}

pub(super) fn fetch_definition() -> ToolDefinition {
    ToolDefinition {
        name: WEB_FETCH_TOOL_NAME.into(),
        description: format!(
            "IMPORTANT: {WEB_FETCH_TOOL_NAME} WILL FAIL for authenticated or private URLs. Before using this tool, check if the URL points to an authenticated service (e.g. Google Docs, Confluence, Jira, GitHub).

- Fetches content from a specified URL and processes it using an AI model
- Takes a URL and a prompt as input
- Fetches the URL content, converts HTML to markdown
- Processes the content with the prompt in a separate request to the current model
- Returns the model's response about the content
- Use this tool when you need to retrieve and analyze web content

Usage notes:
  - The URL must be a fully-formed valid URL
  - HTTP URLs will be automatically upgraded to HTTPS
  - The prompt should describe what information you want to extract from the page
  - This tool is read-only and does not modify any files
  - Results may be summarized if the content is very large
  - Includes a self-cleaning 15-minute cache for faster responses when repeatedly accessing the same URL
  - When a URL redirects to a different host, the tool will inform you and provide the redirect URL in a special format. You should then make a new {WEB_FETCH_TOOL_NAME} request with the redirect URL to fetch the content.
  - For GitHub URLs, prefer using the gh CLI via {BASH_TOOL_NAME} instead (e.g., gh pr view, gh issue view, gh api)."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch content from"
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt to run on the fetched content"
                }
            },
            "required": ["url", "prompt"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn search_definition() -> ToolDefinition {
    let now = time::OffsetDateTime::now_utc();
    let current_month = format!("{} {}", now.month(), now.year());
    ToolDefinition {
        name: WEB_SEARCH_TOOL_NAME.into(),
        description: format!(
            "- Allows you to search the web and use the results to inform responses
- Provides up-to-date information for current events and recent data
- Returns search result titles, links, and available summaries from the configured search service
- Use this tool for accessing information beyond your knowledge cutoff

CRITICAL REQUIREMENT - You MUST follow this:
  - After answering the user's question, you MUST include a \"Sources:\" section at the end of your response
  - In the Sources section, list all relevant URLs from the search results as markdown hyperlinks: [Title](URL)
  - This is MANDATORY - never skip including sources in your response
  - Example format:

    [Your answer here]

    Sources:
    - [Source Title 1](https://example.com/1)
    - [Source Title 2](https://example.com/2)

Usage notes:
  - Domain filtering is supported to include or block specific websites
  - Searching requires a configured search service; an unconfigured service is reported as an error

IMPORTANT - Use the correct year in search queries:
  - The current month is {current_month}. You MUST use this year when searching for recent information, documentation, or current events.
  - Example: If the user asks for \"latest React docs\", search for \"React documentation\" with the current year, NOT last year"
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": MIN_QUERY_LENGTH,
                    "description": "The search query to use"
                },
                "allowed_domains": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Only include search results from these domains"
                },
                "blocked_domains": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Never include search results from these domains"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_LIMIT,
                    "description": format!("The maximum number of results to return (default {DEFAULT_SEARCH_LIMIT})")
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn prepare_fetch(input: &Value) -> Result<(String, Operation, Value), ToolError> {
    let mut parsed: FetchInput = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    if utf16_length(&parsed.url) > MAX_URL_LENGTH {
        return Err(ToolError::InvalidInput(format!(
            "URL exceeds the {MAX_URL_LENGTH}-character limit"
        )));
    }
    let mut url = Url::parse(&parsed.url).map_err(|_| {
        ToolError::Rejected(format!(
            "Error: Invalid URL \"{}\". The URL provided could not be parsed.",
            parsed.url
        ))
    })?;
    if url.scheme() == "http" && !is_loopback(&url) {
        url.set_scheme("https")
            .expect("an HTTP URL can always switch to HTTPS");
    }
    parsed.url = validate_url(url.as_str(), false)?.to_string();
    let normalized_input = json!({"url": parsed.url, "prompt": parsed.prompt});
    Ok((".".into(), Operation::WebFetch(parsed), normalized_input))
}

pub(super) fn prepare_search(input: &Value) -> Result<(String, Operation, Value), ToolError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SearchParameters {
        query: String,
        #[serde(default)]
        allowed_domains: Option<Vec<String>>,
        #[serde(default)]
        blocked_domains: Option<Vec<String>>,
        #[serde(default, deserialize_with = "semantic_integer")]
        limit: Option<usize>,
    }
    let parsed: SearchParameters = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    if parsed.query.trim().is_empty() || utf16_length(&parsed.query) < MIN_QUERY_LENGTH {
        return Err(ToolError::InvalidInput(format!(
            "query must contain at least {MIN_QUERY_LENGTH} characters"
        )));
    }
    if parsed.query.len() > MAX_QUERY_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "query must not exceed {MAX_QUERY_BYTES} bytes"
        )));
    }
    let allowed = parsed.allowed_domains.unwrap_or_default();
    let blocked = parsed.blocked_domains.unwrap_or_default();
    if !allowed.is_empty() && !blocked.is_empty() {
        return Err(ToolError::Rejected(BOTH_DOMAIN_LISTS_ERROR.into()));
    }
    if parsed
        .limit
        .is_some_and(|limit| limit == 0 || limit > MAX_SEARCH_LIMIT)
    {
        return Err(ToolError::InvalidInput(format!(
            "search limit must be between 1 and {MAX_SEARCH_LIMIT} results"
        )));
    }
    let mut normalized_input = json!({"query": parsed.query});
    let domains = if !allowed.is_empty() {
        let domains = normalize_domains("allowed_domains", &allowed)?;
        normalized_input["allowed_domains"] = json!(domains);
        DomainFilter::Allowed(domains)
    } else if !blocked.is_empty() {
        let domains = normalize_domains("blocked_domains", &blocked)?;
        normalized_input["blocked_domains"] = json!(domains);
        DomainFilter::Blocked(domains)
    } else {
        DomainFilter::Any
    };
    if let Some(limit) = parsed.limit {
        normalized_input["limit"] = json!(limit);
    }
    Ok((
        ".".into(),
        Operation::WebSearch(SearchInput {
            query: parsed.query,
            domains,
            limit: parsed.limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
            config: None,
        }),
        normalized_input,
    ))
}

/// Reduces each entry to the lowercase host it names; a leading `*.` or `.` is accepted as
/// another way to say the domain and its subdomains.
fn normalize_domains(field: &str, entries: &[String]) -> Result<Vec<String>, ToolError> {
    entries
        .iter()
        .map(|entry| {
            let domain = entry
                .trim()
                .trim_start_matches("*.")
                .trim_start_matches('.');
            Url::parse(&format!("https://{domain}/"))
                .ok()
                .filter(|url| {
                    url.username().is_empty()
                        && url.password().is_none()
                        && url.port().is_none()
                        && url.path() == "/"
                        && url.query().is_none()
                        && url.fragment().is_none()
                })
                .and_then(|url| Some(url.host_str()?.trim_end_matches('.').to_owned()))
                .filter(|host| !host.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidInput(format!(
                        "{field} entries must be domain names such as example.com, not {entry:?}"
                    ))
                })
        })
        .collect()
}

pub(super) fn validate_search(config: &SearchConfig) -> Result<(), ToolError> {
    validate_url(&config.endpoint, true)?;
    if config
        .api_key_env
        .as_ref()
        .is_some_and(|name| name.is_empty() || name.contains('='))
    {
        return Err(ToolError::InvalidInput(
            "search credentials must name an environment variable".into(),
        ));
    }
    Ok(())
}

/// Claude Code `WebFetchTool.call`: the page comes from the cache or the network, and only the
/// secondary model's answer to the prompt reaches the conversation.
pub(super) async fn fetch(
    input: FetchInput,
    cache: &FetchCache,
    secondary_model: &dyn SecondaryModel,
    cancel: CancellationToken,
) -> Result<ToolResult, ToolError> {
    let markdown = match cache.get(&input.url) {
        Some(markdown) => markdown,
        None => {
            let url = validate_url(&input.url, false)?;
            let page = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
                page = fetch_page(url, FETCH_TIMEOUT) => page?,
            };
            match page {
                FetchedPage::Markdown(markdown) => {
                    cache.insert(&input.url, markdown.clone());
                    markdown
                }
                FetchedPage::Redirect(redirect) => {
                    return Ok(text_result(redirect_message(&redirect, &input.prompt)));
                }
            }
        }
    };
    let prompt = secondary_model_prompt(&truncate_markdown(&markdown), &input.prompt);
    let answer = secondary_model.respond(prompt, cancel).await?;
    Ok(text_result(answer))
}

enum FetchedPage {
    Markdown(Arc<str>),
    Redirect(CrossOriginRedirect),
}

async fn fetch_page(url: Url, timeout: Duration) -> Result<FetchedPage, ToolError> {
    let fetch = async {
        let (content_type, bytes) = match request(url, None, timeout).await? {
            HttpResponse::Body {
                content_type,
                bytes,
            } => (content_type, bytes),
            HttpResponse::Redirect(redirect) => return Ok(FetchedPage::Redirect(redirect)),
        };
        let markdown = page_markdown(&content_type, bytes).await?;
        Ok(FetchedPage::Markdown(markdown.into()))
    };
    tokio::time::timeout(timeout, fetch)
        .await
        .map_err(|_| ToolError::Network("request timed out".into()))?
}

/// Claude Code converts HTML with Turndown and passes other text through unchanged.
async fn page_markdown(content_type: &str, bytes: Vec<u8>) -> Result<String, ToolError> {
    if !(content_type.starts_with("text/")
        || content_type.starts_with("application/json")
        || content_type.starts_with("application/xhtml+xml"))
    {
        return Err(ToolError::Network(
            "unsupported response content type".into(),
        ));
    }
    let body = String::from_utf8(bytes)
        .map_err(|_| ToolError::Network("response is not UTF-8 text".into()))?;
    if !(content_type.starts_with("text/html") || content_type.starts_with("application/xhtml+xml"))
    {
        return Ok(body);
    }
    tokio::task::spawn_blocking(move || html_to_markdown(&body))
        .await
        .map_err(|_| ToolError::Network("HTML conversion did not finish".into()))?
}

fn html_to_markdown(html: &str) -> Result<String, ToolError> {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript"])
        .build()
        .convert(html)
        .map_err(|error| ToolError::Network(format!("cannot convert HTML to Markdown: {error}")))
}

/// Claude Code `applyPromptToMarkdown` keeps the first 100000 characters of a longer page.
fn truncate_markdown(markdown: &str) -> Cow<'_, str> {
    let mut units = 0;
    for (index, character) in markdown.char_indices() {
        units += character.len_utf16();
        if units > MAX_MARKDOWN_LENGTH {
            return Cow::Owned(format!(
                "{}{MARKDOWN_TRUNCATION_NOTICE}",
                &markdown[..index]
            ));
        }
    }
    Cow::Borrowed(markdown)
}

/// Claude Code `makeSecondaryModelPrompt` for a domain that is not preapproved.
fn secondary_model_prompt(markdown: &str, prompt: &str) -> String {
    format!(
        "\nWeb page content:\n---\n{markdown}\n---\n\n{prompt}\n\n{SECONDARY_MODEL_GUIDELINES}\n"
    )
}

/// Claude Code's `REDIRECT DETECTED` result, which asks the model for a new call to the target.
fn redirect_message(redirect: &CrossOriginRedirect, prompt: &str) -> String {
    let status_text = match redirect.status {
        301 => "Moved Permanently",
        308 => "Permanent Redirect",
        307 => "Temporary Redirect",
        _ => "Found",
    };
    format!(
        "REDIRECT DETECTED: The URL redirects to a different host.

Original URL: {}
Redirect URL: {}
Status: {} {status_text}

To complete your request, I need to fetch content from the redirected URL. Please use {WEB_FETCH_TOOL_NAME} again with these parameters:
- url: \"{}\"
- prompt: \"{prompt}\"",
        redirect.original, redirect.target, redirect.status, redirect.target
    )
}

pub(super) async fn search(
    input: SearchInput,
    cancel: CancellationToken,
) -> Result<ToolResult, ToolError> {
    #[derive(Deserialize)]
    struct Response {
        results: Vec<Hit>,
    }
    #[derive(Deserialize)]
    struct Hit {
        title: String,
        url: String,
        #[serde(default)]
        content: String,
    }
    let config = input
        .config
        .ok_or_else(|| ToolError::Network("search is not configured".into()))?;
    let mut url = validate_url(&config.endpoint, true)?;
    url.query_pairs_mut()
        .append_pair("q", &input.query)
        .append_pair("format", "json");
    let key = config
        .api_key_env
        .as_ref()
        .map(|name| {
            std::env::var(name).map_err(|_| {
                ToolError::Network(format!(
                    "search credential environment variable {name} is unavailable"
                ))
            })
        })
        .transpose()?;
    let search = async {
        let (content_type, bytes) = match request(url, key.as_deref(), SEARCH_TIMEOUT).await? {
            HttpResponse::Body {
                content_type,
                bytes,
            } => (content_type, bytes),
            HttpResponse::Redirect(redirect) => {
                return Err(ToolError::Network(format!(
                    "redirect requires a separately approved URL: {}",
                    redirect.target
                )));
            }
        };
        if !content_type.starts_with("application/json") {
            return Err(ToolError::Network(
                "search service did not return JSON".into(),
            ));
        }
        let response: Response = serde_json::from_slice(&bytes).map_err(|_| {
            ToolError::Network("search response has invalid JSON or missing result fields".into())
        })?;
        let mut links = Vec::new();
        for hit in response.results {
            let url = Url::parse(&hit.url)
                .map_err(|_| ToolError::Network("search returned an invalid source URL".into()))?;
            if !matches!(url.scheme(), "https" | "http")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(ToolError::Network(
                    "search returned an unsupported source URL".into(),
                ));
            }
            if input.domains.admits(&url) {
                links.push(SearchLink {
                    title: hit.title,
                    url: url.to_string(),
                    summary: hit.content,
                });
            }
        }
        links.truncate(input.limit);
        Ok(text_result(search_result_text(&input.query, &links)))
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ToolError::Cancelled),
        result = tokio::time::timeout(SEARCH_TIMEOUT, search) => {
            result.map_err(|_| ToolError::Network("request timed out".into()))?
        }
    }
}

/// One hit in Claude Code's `Links:` list, with the summary the configured service provides.
#[derive(Debug, Serialize)]
struct SearchLink {
    title: String,
    url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    summary: String,
}

/// Claude Code `WebSearchTool.mapToolResultToToolResultBlockParam` for one search.
fn search_result_text(query: &str, links: &[SearchLink]) -> String {
    let links = if links.is_empty() {
        NO_LINKS_FOUND.to_owned()
    } else {
        format!(
            "Links: {}",
            serde_json::to_string(links).expect("search links serialize")
        )
    };
    format!("Web search results for query: \"{query}\"\n\n{links}\n\n\n{SOURCES_REMINDER}")
}

fn text_result(content: String) -> ToolResult {
    ToolResult {
        content,
        is_error: false,
        output_ref: None,
    }
}

enum HttpResponse {
    Body {
        content_type: String,
        bytes: Vec<u8>,
    },
    Redirect(CrossOriginRedirect),
}

/// A redirect to another origin, which is never followed without a new approved request.
struct CrossOriginRedirect {
    original: Url,
    target: Url,
    status: u16,
}

async fn request(
    mut url: Url,
    key: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse, ToolError> {
    let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
    policy.timeout = Some(timeout);
    let (client, route) = build_http_client(&url, policy)
        .map_err(|_| ToolError::Network("cannot initialize HTTP client".into()))?;
    for redirects in 0..=MAX_REDIRECTS {
        let mut request = client.get(url.clone()).header(
            reqwest::header::USER_AGENT,
            concat!("Tickloop-Agent/", env!("CARGO_PKG_VERSION")),
        );
        if let Some(key) = key {
            request = request.header("X-API-Key", key);
        }
        let started = Instant::now();
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                NetworkDiagnostic::new("web_request", &url, route, None, started.elapsed()).emit();
                return Err(network_error(error));
            }
        };
        let status = response.status().as_u16();
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ToolError::Network("redirect has no valid Location".into()))?;
            let next = url
                .join(location)
                .map_err(|_| ToolError::Network("redirect target is invalid".into()))?;
            if url.origin() != next.origin() {
                NetworkDiagnostic::new(
                    "web_redirect",
                    &url,
                    route,
                    Some(status),
                    started.elapsed(),
                )
                .emit();
                return Ok(HttpResponse::Redirect(CrossOriginRedirect {
                    original: url,
                    target: next,
                    status,
                }));
            }
            if redirects == MAX_REDIRECTS {
                NetworkDiagnostic::new(
                    "web_redirect",
                    &url,
                    route,
                    Some(status),
                    started.elapsed(),
                )
                .emit();
                return Err(ToolError::Network("too many redirects".into()));
            }
            url = validate_url(next.as_str(), false)?;
            continue;
        }
        if !(200..300).contains(&status) {
            NetworkDiagnostic::new("web_request", &url, route, Some(status), started.elapsed())
                .emit();
        }
        if status == 429 {
            return Err(ToolError::Network(
                "service rate limited the request (HTTP 429)".into(),
            ));
        }
        if status == 401 || status == 403 {
            return Err(ToolError::Network(format!(
                "service access denied (HTTP {status})"
            )));
        }
        if !(200..300).contains(&status) {
            return Err(ToolError::Network(format!(
                "service returned HTTP {status}"
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let too_large = || {
            ToolError::Network(format!(
                "response exceeds {} MB; no content was returned",
                MAX_RESPONSE_BYTES / (1024 * 1024)
            ))
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(HttpResponse::Body {
            content_type,
            bytes,
        });
    }
    unreachable!()
}

fn validate_url(value: &str, is_endpoint: bool) -> Result<Url, ToolError> {
    let url = Url::parse(value)
        .map_err(|_| ToolError::InvalidInput("URL must be a valid HTTP(S) address".into()))?;
    if !(url.scheme() == "https" || url.scheme() == "http" && is_loopback(&url))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || is_endpoint && (url.query().is_some() || url.fragment().is_some())
    {
        return Err(ToolError::InvalidInput("use HTTPS without embedded credentials; HTTP is allowed for loopback; search endpoints cannot contain queries or fragments".into()));
    }
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn network_error(error: reqwest::Error) -> ToolError {
    ToolError::Network(
        if error.is_timeout() {
            "request timed out"
        } else if error.is_connect() {
            "connection failed"
        } else {
            "response transfer failed"
        }
        .into(),
    )
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
