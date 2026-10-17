use std::{path::PathBuf, time::SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub output_ref: Option<PathBuf>,
}

/// What a Chat last saw of one file, following Claude Code's `readFileState` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileReadRecord {
    /// The returned lines joined by `\n`, with `\r\n` normalized and without line numbers.
    pub content: String,
    /// The file's modification time when it was read.
    pub modified: SystemTime,
    /// The first returned line, counted from 1.
    pub offset: usize,
    /// The line limit the read asked for; `None` when it relied on the default.
    pub limit: Option<usize>,
    /// Whether `content` covers the whole file.
    pub is_full_read: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CommandState {
    Running,
    Background,
    Cancelling,
    Completed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandSnapshot {
    pub operation_id: Uuid,
    pub loop_id: Uuid,
    pub command: String,
    /// What the model said the command does; task notifications name the command by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub state: CommandState,
    #[serde(default)]
    pub was_background: bool,
    /// Whether the foreground timeout, rather than the user, moved the command to the background.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backgrounded_on_timeout: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub output_ref: PathBuf,
    pub output_bytes: u64,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchProviderKind {
    Free,
    Brave,
    Tavily,
    Exa,
    #[serde(rename = "serpapi")]
    SerpApi,
    Serper,
    Perplexity,
    #[default]
    Searxng,
}

impl SearchProviderKind {
    fn is_searxng(&self) -> bool {
        *self == Self::Searxng
    }
    pub const ALL: [Self; 8] = [
        Self::Free,
        Self::Brave,
        Self::Tavily,
        Self::Exa,
        Self::SerpApi,
        Self::Serper,
        Self::Perplexity,
        Self::Searxng,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Free => "DuckDuckGo · Free",
            Self::Brave => "Brave",
            Self::Tavily => "Tavily",
            Self::Exa => "Exa",
            Self::SerpApi => "SerpAPI",
            Self::Serper => "Serper",
            Self::Perplexity => "Perplexity",
            Self::Searxng => "SearXNG / compatible JSON",
        }
    }

    pub fn default_endpoint(self) -> &'static str {
        match self {
            Self::Free => "",
            Self::Brave => "https://api.search.brave.com/res/v1/web/search",
            Self::Tavily => "https://api.tavily.com/search",
            Self::Exa => "https://api.exa.ai/search",
            Self::SerpApi => "https://serpapi.com/search.json",
            Self::Serper => "https://google.serper.dev/search",
            Self::Perplexity => "https://api.perplexity.ai/search",
            Self::Searxng => "",
        }
    }

    pub fn credential_env(self) -> Option<&'static str> {
        match self {
            Self::Brave => Some("BRAVE_API_KEY"),
            Self::Tavily => Some("TAVILY_API_KEY"),
            Self::Exa => Some("EXA_API_KEY"),
            Self::SerpApi => Some("SERPAPI_API_KEY"),
            Self::Serper => Some("SERPER_API_KEY"),
            Self::Perplexity => Some("PERPLEXITY_API_KEY"),
            Self::Free | Self::Searxng => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchTestStatus {
    Available,
    AuthenticationFailed,
    UsageLimited,
    RateLimited,
    NetworkFailed,
    TimedOut,
    Challenge,
    NoResults,
    ConfigurationFailed,
    ServiceFailed,
}

impl SearchTestStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::AuthenticationFailed => "access denied",
            Self::UsageLimited => "quota or plan limit",
            Self::RateLimited => "rate limited",
            Self::NetworkFailed => "network failed",
            Self::TimedOut => "timed out",
            Self::Challenge => "verification required",
            Self::NoResults => "no results; unverified",
            Self::ConfigurationFailed => "configuration needed",
            Self::ServiceFailed => "service failed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchTestResult {
    pub status: SearchTestStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    #[serde(default, skip_serializing_if = "SearchProviderKind::is_searxng")]
    pub provider: SearchProviderKind,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl SearchConfig {
    pub fn for_provider(provider: SearchProviderKind) -> Self {
        Self {
            provider,
            endpoint: provider.default_endpoint().into(),
            api_key_env: provider.credential_env().map(str::to_owned),
        }
    }

    pub fn enabled_default() -> Option<Self> {
        Some(Self::default())
    }

    pub fn is_enabled_default(config: &Option<Self>) -> bool {
        config
            .as_ref()
            .is_some_and(|config| *config == Self::default())
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self::for_provider(SearchProviderKind::Free)
    }
}
