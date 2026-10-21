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
use tickloop_agent_base::network::{
    HttpPolicy, NetworkDiagnostic, Reach, build_http_client, check_public_url, not_public_refusal,
};
use tokio_util::sync::CancellationToken;

use super::web_provenance::{Observations, page_links};
use super::web_search::DomainFilter;
use super::web_search::{self, PreparedSearch, SearchRequest};
use super::{
    Operation, ToolDefinition, ToolError, ToolExecution, WebSource, shell::BASH_TOOL_NAME,
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
    pub(super) url: String,
    prompt: String,
}

#[derive(Clone, Debug)]
pub(super) struct SearchInput {
    query: String,
    domains: DomainFilter,
    pub(super) prepared: Option<Arc<PreparedSearch>>,
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
  - Search uses the source selected in /websearch; free search needs no account or API key
  - Do not request more results by passing limit or offset; refine the query and read source pages with WebFetch

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
    Ok((
        ".".into(),
        Operation::WebSearch(SearchInput {
            query: parsed.query,
            domains,
            prepared: None,
        }),
        normalized_input,
    ))
}

/// Reduces each entry to the lowercase host it names; a leading `*.` or `.` is accepted as
/// another way to say the domain and its subdomains.
fn normalize_domains(field: &str, entries: &[String]) -> Result<Vec<String>, ToolError> {
    if entries.len() > 10 {
        return Err(ToolError::InvalidInput(format!(
            "{field} accepts at most 10 domains per call"
        )));
    }
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
    web_search::validate_config(config).map_err(ToolError::InvalidInput)
}

impl SearchInput {
    pub(super) fn request(&self) -> SearchRequest {
        SearchRequest {
            query: self.query.clone(),
            domains: self.domains.clone(),
        }
    }
}

/// Claude Code `WebFetchTool.call`: the page comes from the cache or the network, and only the
/// secondary model's answer to the prompt reaches the conversation.
/// The links of every page it reads and the target of every redirect it reports go to `observed`,
/// which keeps those that may give provenance to the URLs the model fetches next.
pub(super) async fn fetch(
    input: FetchInput,
    cache: &FetchCache,
    observed: &Observations,
    reach: Reach,
    secondary_model: &dyn SecondaryModel,
    cancel: CancellationToken,
) -> Result<ToolExecution, ToolError> {
    let url = validate_url(&input.url, false)?;
    let markdown = match cache.get(&input.url) {
        Some(markdown) => markdown,
        None => {
            let page = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
                page = fetch_page(url.clone(), FETCH_TIMEOUT, reach) => page?,
            };
            match page {
                FetchedPage::Markdown(markdown) => {
                    cache.insert(&input.url, markdown.clone());
                    markdown
                }
                FetchedPage::Redirect(redirect) => {
                    observed.record_redirect(&redirect.original, &redirect.target);
                    return Ok(Ok(text_result(redirect_message(&redirect, &input.prompt))).into());
                }
            }
        }
    };
    observed.record_page(&url, page_links(&markdown, &url));
    let prompt = secondary_model_prompt(&truncate_markdown(&markdown), &input.prompt);
    let answer = secondary_model.respond(prompt, cancel).await?;
    let web_sources = vec![WebSource {
        url: url.to_string(),
        title: markdown
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .unwrap_or(url.as_str())
            .to_owned(),
        snippet: answer.clone(),
    }];
    Ok(ToolExecution {
        result: Ok(text_result(answer)),
        images: Vec::new(),
        web_sources,
    })
}

enum FetchedPage {
    Markdown(Arc<str>),
    Redirect(CrossOriginRedirect),
}

async fn fetch_page(url: Url, timeout: Duration, reach: Reach) -> Result<FetchedPage, ToolError> {
    let fetch = async {
        let (content_type, bytes) = match request(url, None, timeout, reach).await? {
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
) -> Result<ToolExecution, ToolError> {
    let prepared = input
        .prepared
        .as_ref()
        .ok_or_else(|| ToolError::Network("search is not configured".into()))?;
    let request = input.request();
    let response = match prepared.execute(&request, cancel).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(failure) = web_search::failure_details(prepared.kind, &request, &error) {
                return Ok(Ok(ToolResult {
                    content: serde_json::to_string(&failure).expect("search failure serializes"),
                    is_error: true,
                    output_ref: None,
                })
                .into());
            }
            return Err(match error {
                web_search::SearchError::Cancelled => ToolError::Cancelled,
                other => {
                    ToolError::Network(web_search::failure_message(prepared.kind, &request, &other))
                }
            });
        }
    };
    let links = response
        .results
        .into_iter()
        .map(|hit| SearchLink {
            title: hit.title,
            url: hit.url,
            summary: hit.snippet.unwrap_or_default(),
            published_at: hit.published_at,
        })
        .collect::<Vec<_>>();
    let source = response.engine.unwrap_or(response.provider.label());
    let at = time::OffsetDateTime::from(response.searched_at)
        .format(&time::format_description::well_known::Rfc3339)
        .expect("current timestamp has RFC3339 representation");
    let web_sources = links
        .iter()
        .map(|link| WebSource {
            url: link.url.clone(),
            title: link.title.clone(),
            snippet: link.summary.clone(),
        })
        .collect();
    Ok(ToolExecution {
        result: Ok(text_result(format!(
            "Search source: {source}\nSearched at: {at}\n\n{}{}",
            search_result_text(&response.query, &links),
            if response.truncated {
                "\nAdditional results were omitted."
            } else {
                ""
            }
        ))),
        images: Vec::new(),
        web_sources,
    })
}

/// One hit in Claude Code's `Links:` list, with the summary the configured service provides.
#[derive(Debug, Serialize)]
struct SearchLink {
    title: String,
    url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
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

/// Refuses a URL that `reach` does not admit; a host name is checked again by every address it
/// resolves to when it is requested.
pub(super) fn check_reach(url: &str, reach: Reach) -> Result<(), ToolError> {
    if reach == Reach::Anywhere {
        return Ok(());
    }
    let url = validate_url(url, false)?;
    check_public_url(&url).map_err(|refusal| ToolError::Rejected(refusal.to_string()))
}

/// Where a redirect from `url` to `location` leads, refused when `reach` does not admit it.
fn redirect_target(url: &Url, location: &str, reach: Reach) -> Result<Url, ToolError> {
    let next = url
        .join(location)
        .map_err(|_| ToolError::Network("redirect target is invalid".into()))?;
    check_reach(next.as_str(), reach)?;
    Ok(next)
}

async fn request(
    mut url: Url,
    key: Option<&str>,
    timeout: Duration,
    reach: Reach,
) -> Result<HttpResponse, ToolError> {
    let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
    policy.timeout = Some(timeout);
    policy.reach = reach;
    let (client, route) = build_http_client(&url, policy)
        .map_err(|_| ToolError::Network("cannot initialize HTTP client".into()))?;
    for redirects in 0..=MAX_REDIRECTS {
        check_reach(url.as_str(), reach)?;
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
                NetworkDiagnostic::new("web_request", &url, route, None, started.elapsed())
                    .emit_for(reach);
                return Err(match not_public_refusal(&error) {
                    Some(refusal) => ToolError::Rejected(refusal.to_string()),
                    None => network_error(error),
                });
            }
        };
        let status = response.status().as_u16();
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ToolError::Network("redirect has no valid Location".into()))?;
            let next = redirect_target(&url, location, reach)?;
            if url.origin() != next.origin() {
                NetworkDiagnostic::new(
                    "web_redirect",
                    &url,
                    route,
                    Some(status),
                    started.elapsed(),
                )
                .emit_for(reach);
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
                .emit_for(reach);
                return Err(ToolError::Network("too many redirects".into()));
            }
            url = validate_url(next.as_str(), false)?;
            continue;
        }
        if !(200..300).contains(&status) {
            NetworkDiagnostic::new("web_request", &url, route, Some(status), started.elapsed())
                .emit_for(reach);
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
