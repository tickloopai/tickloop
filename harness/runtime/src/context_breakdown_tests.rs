use std::sync::Arc;

use serde_json::{Value, json};
use tickloop_harness_protocol::mcp::McpServerState;
use tickloop_harness_protocol::{ChatId, Protocol};

use super::*;
use crate::host_tools::{HostToolPermission, HostToolProvider};
use crate::tool_exposure::{ExposureTool, UnavailableServer};

/// Counts whitespace-separated words, so expected counts are easy to read.
struct WordCounter;

impl ContextTokenCounter for WordCounter {
    fn name(&self) -> &'static str {
        "words"
    }

    fn text_tokens(&self, text: &str) -> usize {
        text.split_whitespace().count()
    }
}

fn message(role: Role, content: Vec<Content>) -> Message {
    Message { role, content }
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

fn definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object"}),
    }
}

fn parts(messages: Vec<Message>, search: Option<SearchParts>) -> ContextParts {
    ContextParts {
        system_prompt: message(Role::System, vec![text("one two three four")]),
        system_tools: vec![definition("Read", "reads files")],
        skill_tool: Some(definition("Skill", "- alpha [project]: helps")),
        skills: vec![("alpha".into(), "- alpha [project]: helps".into())],
        mcp_tools: vec![("docs".into(), definition("mcp__docs__find", "finds docs"))],
        user_context: message(Role::User, vec![text("instructions here")]),
        instruction_files: vec![("/work/AGENTS.md".into(), "instructions here".into())],
        messages,
        search,
        window: Some(1_000),
        compaction_trigger: Some(800),
        reported_total: None,
    }
}

fn search_parts(mode: ToolSearchMode) -> SearchParts {
    SearchParts {
        mode,
        pool: vec![
            (
                "market".into(),
                definition("mcp__market__quote", "a live quote"),
            ),
            ("market".into(), definition("mcp__market__news", "news")),
            (
                "office".into(),
                definition("mcp__office__open", "opens a document now"),
            ),
        ],
        unavailable: vec![UnavailableToolServer {
            name: "figma".into(),
            state: McpServerState::NeedsLogin,
        }],
        deferred: vec![definition("mcp__market__quote", "a live quote")],
    }
}

fn category(breakdown: &ContextBreakdown, category: ContextCategory) -> u64 {
    breakdown
        .categories
        .iter()
        .find(|usage| usage.category == category)
        .map_or(0, |usage| usage.tokens)
}

fn category_sum(breakdown: &ContextBreakdown) -> u64 {
    breakdown.categories.iter().map(|usage| usage.tokens).sum()
}

#[test]
fn before_a_request_the_counted_categories_make_the_total_and_leave_free_space_and_the_buffer() {
    let breakdown =
        parts(vec![message(Role::User, vec![text("hello there")])], None).count(&WordCounter);
    assert!(!breakdown.calibrated);
    assert_eq!(breakdown.tokenizer, "words");
    assert_eq!(category(&breakdown, ContextCategory::SystemPrompt), 4 + 3);
    assert_eq!(
        category(&breakdown, ContextCategory::SystemTools),
        1 + 2 + 1
    );
    assert_eq!(category(&breakdown, ContextCategory::McpTools), 1 + 2 + 1);
    assert_eq!(
        category(&breakdown, ContextCategory::InstructionFiles),
        2 + 3
    );
    assert_eq!(category(&breakdown, ContextCategory::Messages), 2 + 3);
    assert_eq!(category(&breakdown, ContextCategory::ToolSearchResults), 0);
    assert_eq!(breakdown.used_tokens, category_sum(&breakdown));
    assert_eq!(breakdown.autocompact_buffer_tokens, Some(200));
    assert_eq!(
        breakdown.free_tokens,
        Some(1_000 - breakdown.used_tokens - 200)
    );
    assert_eq!(breakdown.instruction_files[0].name, "/work/AGENTS.md");
    assert_eq!(breakdown.skills[0].name, "alpha");
    assert!(breakdown.tool_search.is_none());
}

#[test]
fn a_measured_total_scales_every_category_and_the_buffer_never_overlaps_used_space() {
    let mut measured = parts(vec![message(Role::User, vec![text("hello there")])], None);
    measured.reported_total = Some(900);
    let breakdown = measured.count(&WordCounter);
    assert!(breakdown.calibrated);
    assert_eq!(breakdown.used_tokens, 900);
    assert_eq!(category_sum(&breakdown), 900, "categories add up exactly");
    assert_eq!(breakdown.autocompact_buffer_tokens, Some(100));
    assert_eq!(breakdown.free_tokens, Some(0));
    let unscaled =
        parts(vec![message(Role::User, vec![text("hello there")])], None).count(&WordCounter);
    assert!(
        category(&breakdown, ContextCategory::SystemPrompt)
            > category(&unscaled, ContextCategory::SystemPrompt)
    );

    let mut overflowing = parts(Vec::new(), None);
    overflowing.reported_total = Some(1_500);
    let breakdown = overflowing.count(&WordCounter);
    assert_eq!(breakdown.autocompact_buffer_tokens, Some(0));
    assert_eq!(breakdown.free_tokens, Some(0));
}

#[test]
fn search_tools_calls_and_their_results_move_out_of_messages() {
    let messages = vec![
        message(
            Role::Assistant,
            vec![Content::ToolUse {
                id: "call-search".into(),
                name: SEARCH_TOOLS.into(),
                input: json!({"query": "quote stock"}),
            }],
        ),
        message(
            Role::User,
            vec![
                Content::ToolResult {
                    tool_use_id: "call-search".into(),
                    content: tickloop_harness_protocol::ToolResultContent::Text(
                        "found quote".into(),
                    ),
                    is_error: false,
                },
                text("hello there"),
            ],
        ),
        message(
            Role::User,
            vec![Content::ToolResult {
                tool_use_id: "call-read".into(),
                content: tickloop_harness_protocol::ToolResultContent::Text("file text".into()),
                is_error: false,
            }],
        ),
    ];
    let breakdown =
        parts(messages, Some(search_parts(ToolSearchMode::SearchTools))).count(&WordCounter);
    assert_eq!(
        breakdown.tool_search_results,
        vec![ContextItem {
            name: "SearchTools quote stock".into(),
            tokens: 1 + 2 + 2,
        }]
    );
    assert_eq!(category(&breakdown, ContextCategory::ToolSearchResults), 5);
    assert_eq!(
        category(&breakdown, ContextCategory::Messages),
        3 + (3 + 2) + (3 + 2)
    );
    assert_eq!(
        breakdown.messages,
        vec![
            ContextItem {
                name: USER_MESSAGES.into(),
                tokens: 8,
            },
            ContextItem {
                name: ASSISTANT_MESSAGES.into(),
                tokens: 3,
            },
            ContextItem {
                name: TOOL_RESULTS.into(),
                tokens: 2,
            },
        ]
    );
}

#[test]
fn native_search_records_count_with_each_loaded_definition_once() {
    let result = |id: &str| Content::ProtocolData {
        protocol: Protocol::Messages,
        value: json!({
            "type": "tool_search_tool_result",
            "tool_use_id": id,
            "content": {"tool_references": [{"tool_name": "mcp__market__quote"}]},
        }),
    };
    let messages = vec![message(
        Role::Assistant,
        vec![result("first"), result("second")],
    )];
    let breakdown = parts(messages, Some(search_parts(ToolSearchMode::Native))).count(&WordCounter);
    let names = breakdown
        .tool_search_results
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "mcp__market__quote (loaded definition)")
            .count(),
        1
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "Tool search found: mcp__market__quote.")
            .count(),
        2
    );
    assert_eq!(category(&breakdown, ContextCategory::Messages), 3);
}

#[test]
fn the_search_pool_groups_by_server_and_stays_out_of_the_total() {
    let without = parts(Vec::new(), None).count(&WordCounter);
    let with =
        parts(Vec::new(), Some(search_parts(ToolSearchMode::SearchTools))).count(&WordCounter);
    assert_eq!(with.used_tokens, without.used_tokens);
    let pool = with.tool_search.unwrap();
    assert_eq!(pool.mode, ToolSearchMode::SearchTools);
    assert_eq!(pool.tool_count(), 3);
    assert_eq!(
        pool.servers
            .iter()
            .map(|server| (server.server.as_str(), server.tools.len()))
            .collect::<Vec<_>>(),
        vec![("market", 2), ("office", 1)],
        "servers holding the most tokens come first"
    );
    assert_eq!(pool.servers[0].tools[0].name, "mcp__market__quote");
    assert_eq!(pool.unavailable_servers[0].name, "figma");
}

struct SearchCatalog {
    tools: Vec<ExposureTool>,
}

#[async_trait::async_trait]
impl HostToolProvider for SearchCatalog {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| tool.exposure != ToolExposure::Hidden)
            .map(|tool| tool.definition.clone())
            .collect()
    }

    fn discovery_tools(&self) -> Vec<ExposureTool> {
        self.tools.clone()
    }

    fn unavailable_tool_servers(&self) -> Vec<UnavailableServer> {
        vec![UnavailableServer {
            name: "figma".into(),
            state: McpServerState::NeedsLogin,
        }]
    }

    fn has_tool_search(&self) -> bool {
        true
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ThirdParty
    }

    async fn execute(
        &self,
        _: ChatId,
        _: &str,
        _: Value,
        _: tokio_util::sync::CancellationToken,
    ) -> tickloop_harness_protocol::ToolResult {
        unreachable!("counting context never calls a tool")
    }
}

fn exposure_tool(server: &str, name: &str, exposure: ToolExposure) -> ExposureTool {
    let full_name = format!("mcp__{server}__{name}");
    ExposureTool {
        server: server.into(),
        original_name: name.into(),
        definition: definition(&full_name, &format!("{name} for {server}")),
        original_schema: json!({"type": "object"}),
        annotations: json!({}),
        exposure,
    }
}

async fn agent_with_catalog() -> (Agent, ChatId, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_harness_test_support::config(),
        Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![])),
    );
    agent
        .install_host_tools(Arc::new(SearchCatalog {
            tools: vec![
                exposure_tool("market", "quote", ToolExposure::Search),
                exposure_tool("market", "news", ToolExposure::Search),
                exposure_tool("office", "open", ToolExposure::Direct),
                exposure_tool("vault", "secret", ToolExposure::Hidden),
            ],
        }))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    (agent, chat, directory)
}

#[tokio::test]
async fn a_chat_sends_direct_tools_searches_the_rest_and_never_names_hidden_ones() {
    let (agent, chat, _directory) = agent_with_catalog().await;
    let breakdown = agent.context_breakdown(chat).await.unwrap();
    assert_eq!(breakdown.used_tokens, category_sum(&breakdown));
    assert_eq!(
        breakdown
            .mcp_tools
            .iter()
            .map(|server| (server.server.as_str(), server.tools.len()))
            .collect::<Vec<_>>(),
        vec![("office", 1)]
    );
    let pool = breakdown.tool_search.as_ref().unwrap();
    assert_eq!(pool.mode, ToolSearchMode::SearchTools);
    assert_eq!(pool.servers.len(), 1);
    assert_eq!(pool.servers[0].server, "market");
    assert_eq!(pool.servers[0].tools.len(), 2);
    assert!(pool.tokens() > 0);
    assert_eq!(
        pool.unavailable_servers[0].state,
        McpServerState::NeedsLogin
    );
    for name in FOLD_TOOLS {
        assert!(
            breakdown.system_tools.iter().any(|tool| tool.name == name),
            "{name} is a system tool in SearchTools mode"
        );
    }
    assert!(
        !serde_json::to_string(&breakdown)
            .unwrap()
            .contains("secret"),
        "a hidden tool is never named"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_subagent_counts_only_the_tools_its_ceiling_grants() {
    let (agent, chat, _directory) = agent_with_catalog().await;
    let mut chats = agent.inner.chats.lock().await;
    let child = chats.get_mut(&chat).unwrap();
    child.snapshot.delegation = Some(tickloop_harness_protocol::DelegationMetadata {
        delegation_id: uuid::Uuid::new_v4(),
        parent_chat_id: chat,
        parent_loop_id: uuid::Uuid::new_v4().into(),
        parent_run_id: uuid::Uuid::new_v4().into(),
        parent_call_id: "call-subagent".into(),
        ancestors: Vec::new(),
        task: String::new(),
        ceiling: tickloop_harness_protocol::DelegationCeiling {
            folder: child.snapshot.folder.clone(),
            additional_directories: Vec::new(),
            policy: child.snapshot.policy.clone(),
            session_denials: None,
            host_context: None,
            host_tools: [("mcp__market__quote".to_owned(), Default::default())].into(),
            tick_instances: Vec::new(),
        },
        trusted_user_inputs: None,
        trusted_reply_history: None,
        trusted_goal: None,
    });
    let child = chats.get(&chat).unwrap();
    let breakdown = agent
        .context_parts(child, &[], &[], "2026-10-27")
        .unwrap()
        .count(&WordCounter);
    drop(chats);
    assert!(
        breakdown.mcp_tools.is_empty(),
        "office is outside the ceiling"
    );
    let pool = breakdown.tool_search.unwrap();
    assert_eq!(pool.tool_count(), 1);
    assert_eq!(pool.servers[0].tools[0].name, "mcp__market__quote");
    agent.shutdown().await;
}
