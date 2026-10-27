//! What a Chat's current context holds by category, as `/context` shows it, and the MCP tools
//! that only tool search reaches, which take no context until loaded.

use serde::{Deserialize, Serialize};

use crate::mcp::McpServerState;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextBreakdown {
    /// The tokenizer the categories and items were counted with.
    pub tokenizer: String,
    pub context_window_tokens: Option<u64>,
    /// The current context the status line shows when `calibrated`; before the Chat's first
    /// request of its model, the counted sum of everything the next request would send.
    pub used_tokens: u64,
    /// Whether `used_tokens` is anchored to the input a service reported for a request.
    pub calibrated: bool,
    /// The categories holding tokens, in display order; they sum to `used_tokens`.
    pub categories: Vec<ContextCategoryUsage>,
    /// The part of the window kept free by automatic compaction, which starts beyond it.
    pub autocompact_buffer_tokens: Option<u64>,
    pub free_tokens: Option<u64>,
    pub system_tools: Vec<ContextItem>,
    /// MCP tools sent directly with each request, by server.
    pub mcp_tools: Vec<ContextToolServer>,
    /// Instruction files by path.
    pub instruction_files: Vec<ContextItem>,
    pub skills: Vec<ContextItem>,
    pub tool_search_results: Vec<ContextItem>,
    /// What the user typed, assistant messages and tool results.
    pub messages: Vec<ContextItem>,
    /// User-role messages the host wrote, such as MCP server instructions and Skill guidance, by
    /// source; they count under Messages too.
    #[serde(default)]
    pub host_notes: Vec<ContextItem>,
    /// Present when the Chat reaches MCP tools through tool search.
    pub tool_search: Option<ToolSearchPool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCategory {
    SystemPrompt,
    SystemTools,
    McpTools,
    InstructionFiles,
    Skills,
    ToolSearchResults,
    Messages,
    /// What the service reported beyond the count by more than the count can explain, such as
    /// definitions a vendor's tool search loads.
    Unattributed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextCategoryUsage {
    pub category: ContextCategory,
    pub tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextItem {
    pub name: String,
    pub tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextToolServer {
    pub server: String,
    pub tools: Vec<ContextItem>,
}

impl ContextToolServer {
    pub fn tokens(&self) -> u64 {
        self.tools.iter().map(|tool| tool.tokens).sum()
    }
}

/// The MCP tools reachable only through tool search: their definitions, counted as a load would
/// send them, take no context until the model loads them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolSearchPool {
    pub mode: ToolSearchMode,
    pub servers: Vec<ContextToolServer>,
    /// Servers whose tools cannot be searched yet.
    pub unavailable_servers: Vec<UnavailableToolServer>,
}

impl ToolSearchPool {
    pub fn tool_count(&self) -> usize {
        self.servers.iter().map(|server| server.tools.len()).sum()
    }

    pub fn tokens(&self) -> u64 {
        self.servers.iter().map(ContextToolServer::tokens).sum()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchMode {
    /// The vendor searches the deferred definitions sent with each request.
    Native,
    /// The model searches with SearchTools, DescribeTools and ExecuteTools.
    SearchTools,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnavailableToolServer {
    pub name: String,
    pub state: McpServerState,
}
