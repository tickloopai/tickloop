use std::sync::Arc;

use serde_json::{Value, json};
use tickloop_harness_protocol::mcp::McpServerState;
use tickloop_harness_protocol::{ChatId, Protocol};

use super::*;
use crate::Chat;
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
        message_labels: vec![None; messages.len()],
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
        protocol: Protocol::Messages,
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
fn a_measured_total_close_to_the_count_scales_every_category_exactly() {
    let counted =
        parts(vec![message(Role::User, vec![text("hello there")])], None).count(&WordCounter);
    for reported in [
        counted.used_tokens * 110 / 100,
        counted.used_tokens * 60 / 100,
    ] {
        let mut measured = parts(vec![message(Role::User, vec![text("hello there")])], None);
        measured.reported_total = Some(reported);
        let breakdown = measured.count(&WordCounter);
        assert!(breakdown.calibrated);
        assert_eq!(breakdown.used_tokens, reported);
        assert_eq!(
            category_sum(&breakdown),
            reported,
            "categories add up exactly"
        );
        assert_eq!(category(&breakdown, ContextCategory::Unattributed), 0);
    }
}

#[test]
fn a_measured_total_far_beyond_the_count_stands_apart_and_never_scales_the_pool() {
    let counted = parts(Vec::new(), Some(search_parts(ToolSearchMode::Native))).count(&WordCounter);
    let mut measured = parts(Vec::new(), Some(search_parts(ToolSearchMode::Native)));
    measured.reported_total = Some(900);
    let breakdown = measured.count(&WordCounter);
    assert_eq!(breakdown.used_tokens, 900);
    assert_eq!(category_sum(&breakdown), 900);
    assert_eq!(
        category(&breakdown, ContextCategory::Unattributed),
        900 - counted.used_tokens
    );
    for category_name in [ContextCategory::SystemPrompt, ContextCategory::Messages] {
        assert_eq!(
            category(&breakdown, category_name),
            category(&counted, category_name),
            "{category_name:?} keeps its count"
        );
    }
    assert_eq!(
        breakdown.tool_search, counted.tool_search,
        "the pool is never scaled"
    );
    assert_eq!(breakdown.autocompact_buffer_tokens, Some(100));
    assert_eq!(breakdown.free_tokens, Some(0));

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

/// Native search over Responses, where each deferred tool's name and description count.
fn responses_search_parts() -> SearchParts {
    SearchParts {
        protocol: Protocol::Responses,
        ..search_parts(ToolSearchMode::Native)
    }
}

#[test]
fn responses_native_search_counts_its_index_by_server_in_context() {
    let without = parts(Vec::new(), None).count(&WordCounter);
    let breakdown = parts(Vec::new(), Some(responses_search_parts())).count(&WordCounter);
    let index = (1 + 3) + (1 + 1) + (1 + 4);
    assert_eq!(
        category(&breakdown, ContextCategory::ToolSearchIndex),
        index
    );
    assert_eq!(breakdown.used_tokens, without.used_tokens + index);
    assert_eq!(
        breakdown
            .tool_search_index
            .iter()
            .map(|server| (server.server.as_str(), server.tokens()))
            .collect::<Vec<_>>(),
        vec![("market", 4 + 2), ("office", 5)]
    );
    let order = breakdown
        .categories
        .iter()
        .map(|usage| usage.category)
        .collect::<Vec<_>>();
    assert_eq!(
        order[order
            .iter()
            .position(|category| *category == ContextCategory::McpTools)
            .unwrap()
            + 1],
        ContextCategory::ToolSearchIndex,
        "the index follows the MCP tools"
    );
    assert_eq!(
        breakdown.tool_search.unwrap().tokens(),
        3 * (1 + 1) + 3 + 1 + 4,
        "the pool still shows whole definitions"
    );

    let mut measured = parts(Vec::new(), Some(responses_search_parts()));
    measured.reported_total = Some(breakdown.used_tokens + 2);
    let measured = measured.count(&WordCounter);
    assert_eq!(category(&measured, ContextCategory::Unattributed), 0);
    assert_eq!(category_sum(&measured), breakdown.used_tokens + 2);
}

#[test]
fn messages_native_search_and_search_tools_count_no_index() {
    for search in [
        search_parts(ToolSearchMode::Native),
        SearchParts {
            protocol: Protocol::Responses,
            ..search_parts(ToolSearchMode::SearchTools)
        },
    ] {
        let breakdown = parts(Vec::new(), Some(search)).count(&WordCounter);
        assert_eq!(category(&breakdown, ContextCategory::ToolSearchIndex), 0);
        assert!(breakdown.tool_search_index.is_empty());
        assert!(breakdown.tool_search.is_some());
    }
}

#[test]
fn a_native_search_index_like_chat_01a17fd4_explains_the_service_total() {
    let words = |count: usize| vec!["word"; count].join(" ");
    let mut search = responses_search_parts();
    search.pool = (0..180)
        .map(|index| {
            (
                "longbridge".to_owned(),
                definition(&format!("mcp__longbridge__tool_{index}"), &words(96)),
            )
        })
        .collect();
    let mut measured = parts(
        vec![message(Role::User, vec![text(&words(26_950))])],
        Some(search),
    );
    measured.reported_total = Some(44_700);
    let breakdown = measured.count(&WordCounter);
    assert_eq!(category(&breakdown, ContextCategory::Unattributed), 0);
    let index = category(&breakdown, ContextCategory::ToolSearchIndex);
    assert!(
        (17_460 * 99 / 100..=17_460 * 102 / 100).contains(&index),
        "the index scales by about 1: {index}"
    );
    assert_eq!(category_sum(&breakdown), 44_700);
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
            .map(|tool| (*tool.definition).clone())
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
        server: Some(server.into()),
        original_name: name.into(),
        definition: Arc::new(definition(&full_name, &format!("{name} for {server}"))),
        original_schema: Arc::new(json!({"type": "object"})),
        annotations: Arc::new(json!({})),
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
    let market = pool
        .servers
        .iter()
        .find(|server| server.server == "market")
        .unwrap();
    assert_eq!(market.tools.len(), 2);
    assert_eq!(
        pool.servers.len(),
        2,
        "market and the runtime's EnterPlanMode"
    );
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
        .context_parts(child, &[], "2026-10-27")
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

fn push(
    chat: &mut Chat,
    role: Role,
    body: &str,
    note: Option<tickloop_harness_protocol::HostNote>,
) {
    let index = chat.snapshot.messages.len();
    chat.snapshot.messages.push(message(role, vec![text(body)]));
    chat.snapshot
        .message_ids
        .insert(index, tickloop_ids::generate());
    if let Some(note) = note {
        chat.snapshot.host_notes.insert(index, note);
    }
}

fn mcp_instructions(server: &str) -> tickloop_harness_protocol::HostNote {
    tickloop_harness_protocol::HostNote::Guidance(GuidanceNote::McpInstructions {
        server: server.into(),
        digest: "sha256:0".into(),
    })
}

fn skill_guidance(name: &str) -> tickloop_harness_protocol::HostNote {
    tickloop_harness_protocol::HostNote::Guidance(GuidanceNote::Skill {
        name: name.into(),
        digest: "sha256:1".into(),
        truncated: false,
    })
}

fn item_tokens(items: &[ContextItem], name: &str) -> u64 {
    items
        .iter()
        .find(|item| item.name == name)
        .map_or(0, |item| item.tokens)
}

/// The shape of a fresh Chat in which the user typed only a greeting: the host had already
/// written the MCP server instructions and the tickloop Skill's guidance into its history.
#[tokio::test]
async fn host_notes_count_apart_from_what_the_user_typed() {
    let (agent, chat_id, _directory) = agent_with_catalog().await;
    let mut chats = agent.inner.chats.lock().await;
    let chat = chats.get_mut(&chat_id).unwrap();
    push(
        chat,
        Role::User,
        &"server says use quotes ".repeat(50),
        Some(mcp_instructions("longbridge")),
    );
    push(
        chat,
        Role::User,
        &"skill guidance words ".repeat(200),
        Some(skill_guidance("tickloop")),
    );
    push(chat, Role::User, "你好", None);
    push(chat, Role::Assistant, "Hello there", None);
    let chat = chats.get(&chat_id).unwrap();
    let breakdown = agent
        .context_parts(chat, &[], "2026-10-27")
        .unwrap()
        .count(&WordCounter);
    drop(chats);
    assert_eq!(item_tokens(&breakdown.messages, USER_MESSAGES), 1 + 3);
    assert_eq!(item_tokens(&breakdown.messages, ASSISTANT_MESSAGES), 2 + 3);
    assert_eq!(
        item_tokens(
            &breakdown.host_notes,
            "MCP server instructions · longbridge"
        ),
        200 + 3
    );
    assert_eq!(
        item_tokens(&breakdown.host_notes, "Skill guidance · tickloop"),
        600 + 3
    );
    assert_eq!(
        category(&breakdown, ContextCategory::Messages),
        4 + 5 + 203 + 603,
        "Messages holds the notes and nothing the other categories count"
    );
    assert!(category(&breakdown, ContextCategory::InstructionFiles) > 0);
    agent.shutdown().await;
}

#[tokio::test]
async fn host_notes_keep_their_labels_after_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_harness_test_support::config(),
        Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![
            tickloop_harness_test_support::text("Earlier reply"),
            tickloop_harness_test_support::text("A short summary"),
        ])),
    );
    let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat_id, "Earlier question".into())
        .await
        .unwrap();
    agent.quiesce(chat_id).await;
    agent.compact(chat_id).await.unwrap();
    let mut chats = agent.inner.chats.lock().await;
    let chat = chats.get_mut(&chat_id).unwrap();
    push(
        chat,
        Role::User,
        "instructions from the server",
        Some(mcp_instructions("longbridge")),
    );
    push(chat, Role::User, "你好", None);
    let chat = chats.get(&chat_id).unwrap();
    let breakdown = agent
        .context_parts(chat, &[], "2026-10-27")
        .unwrap()
        .count(&WordCounter);
    drop(chats);
    assert_eq!(
        item_tokens(
            &breakdown.host_notes,
            "MCP server instructions · longbridge"
        ),
        4 + 3
    );
    assert!(item_tokens(&breakdown.host_notes, "Compaction summary") > 0);
    assert_eq!(item_tokens(&breakdown.messages, USER_MESSAGES), 1 + 3);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_draft_counts_its_first_request_without_creating_a_chat() {
    let (agent, existing, directory) = agent_with_catalog().await;
    let chats_before = agent.inner.chats.lock().await.len();
    let breakdown = agent
        .draft_context_breakdown(
            Some(directory.path().into()),
            tickloop_harness_protocol::ContextDraft {
                model: tickloop_harness_test_support::config_with_window(200_000),
                execution_mode: tickloop_harness_protocol::ExecutionMode::Plan,
                search_config: None,
                task_tools_enabled: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        agent.inner.chats.lock().await.len(),
        chats_before,
        "no Chat is created"
    );
    assert!(!breakdown.calibrated);
    assert_eq!(breakdown.used_tokens, category_sum(&breakdown));
    assert_eq!(category(&breakdown, ContextCategory::Messages), 0);
    assert!(category(&breakdown, ContextCategory::SystemPrompt) > 0);
    assert!(category(&breakdown, ContextCategory::InstructionFiles) > 0);
    assert!(
        breakdown
            .system_tools
            .iter()
            .any(|tool| tool.name == "ExitPlanMode")
    );
    let pool = breakdown.tool_search.unwrap();
    assert_eq!(
        pool.tool_count(),
        3,
        "the catalog the host offers now, and EnterPlanMode"
    );
    assert!(breakdown.free_tokens.is_some() && breakdown.autocompact_buffer_tokens.is_some());
    let _ = existing;
    agent.shutdown().await;
}

async fn draft_breakdown(
    agent: &Agent,
    folder: &std::path::Path,
    model: tickloop_harness_protocol::ModelConfig,
) -> ContextBreakdown {
    agent
        .draft_context_breakdown(
            Some(folder.into()),
            tickloop_harness_protocol::ContextDraft {
                model,
                execution_mode: tickloop_harness_protocol::ExecutionMode::default(),
                search_config: None,
                task_tools_enabled: false,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn an_openai_native_search_model_counts_the_index_of_the_tools_it_defers() {
    let (agent, _chat, directory) = agent_with_catalog().await;
    let catalog = tickloop_harness_model::cards::ModelCatalog::defaults();
    let sol = catalog.get("openai-codex", "gpt-6.1-sol").unwrap();
    let breakdown = draft_breakdown(&agent, directory.path(), sol).await;
    let counter = &*tickloop_harness_model::context_tokens::TOKEN_COUNTER;
    let expected = ["quote", "news"]
        .iter()
        .map(|name| {
            native_tool_search::deferred_tool_context_tokens(
                Protocol::Responses,
                counter,
                &definition(
                    &format!("mcp__market__{name}"),
                    &format!("{name} for market"),
                ),
            ) as u64
        })
        .sum::<u64>();
    assert!(expected > 0);
    let market = breakdown
        .tool_search_index
        .iter()
        .find(|server| server.server == "market")
        .unwrap();
    assert_eq!(market.tokens(), expected);
    assert_eq!(market.tools.len(), 2);
    assert_eq!(
        breakdown.tool_search_index.len(),
        2,
        "market and the runtime's EnterPlanMode"
    );
    assert_eq!(breakdown.used_tokens, category_sum(&breakdown));
    assert_eq!(breakdown.tool_search.unwrap().mode, ToolSearchMode::Native);

    let opus = catalog.get("anthropic-api", "claude-opus-5-5").unwrap();
    let breakdown = draft_breakdown(&agent, directory.path(), opus).await;
    assert_eq!(category(&breakdown, ContextCategory::ToolSearchIndex), 0);
    assert!(breakdown.tool_search_index.is_empty());
    agent.shutdown().await;
}

/// EnterPlanMode waits behind tool search like an MCP tool: a Chat without MCP servers still has
/// a search pool, and the tool counts there and in the native index, never as a system tool.
#[tokio::test]
async fn enter_plan_mode_counts_in_the_search_pool_from_the_runtimes_own_source() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_harness_test_support::config(),
        Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let breakdown = agent.context_breakdown(chat).await.unwrap();
    let names = |items: &[ContextItem]| {
        items
            .iter()
            .map(|item| item.name.clone())
            .collect::<Vec<_>>()
    };
    assert!(!names(&breakdown.system_tools).contains(&"EnterPlanMode".to_owned()));
    assert!(names(&breakdown.system_tools).contains(&"ExitPlanMode".to_owned()));
    for name in FOLD_TOOLS {
        assert!(
            names(&breakdown.system_tools).contains(&name.to_owned()),
            "{name}"
        );
    }
    let pool = breakdown.tool_search.as_ref().unwrap();
    assert_eq!(pool.mode, ToolSearchMode::SearchTools);
    assert_eq!(pool.servers.len(), 1);
    assert_eq!(pool.servers[0].server, RUNTIME_TOOL_SOURCE);
    assert_eq!(names(&pool.servers[0].tools), ["EnterPlanMode"]);

    let catalog = tickloop_harness_model::cards::ModelCatalog::defaults();
    let sol = catalog.get("openai-codex", "gpt-6.1-sol").unwrap();
    let breakdown = draft_breakdown(&agent, directory.path(), sol).await;
    assert_eq!(breakdown.tool_search_index.len(), 1);
    assert_eq!(breakdown.tool_search_index[0].server, RUNTIME_TOOL_SOURCE);
    assert_eq!(
        names(&breakdown.tool_search_index[0].tools),
        ["EnterPlanMode"]
    );
    assert!(category(&breakdown, ContextCategory::ToolSearchIndex) > 0);
    assert!(
        !names(&breakdown.system_tools)
            .iter()
            .any(|name| FOLD_TOOLS.contains(&name.as_str())),
        "native search needs no Fold tools"
    );
    assert_eq!(breakdown.used_tokens, category_sum(&breakdown));
    agent.shutdown().await;
}

#[tokio::test]
async fn the_autocompact_buffer_ends_where_compaction_starts_under_the_window_cap() {
    let directory = tempfile::tempdir().unwrap();
    for cap in [None, Some(100_000)] {
        let mut options = crate::local_options(
            tickloop_harness_test_support::config_with_window(200_000),
            None,
            tickloop_harness_protocol::AgentHostOptions::embedded_defaults(),
        );
        options.settings = Some(Arc::new(
            tickloop_harness_policy::settings::MemorySettingsStore::new(
                tickloop_harness_policy::settings::SettingsSnapshot {
                    auto_compact_window: cap,
                    ..Default::default()
                },
            ),
        ));
        let agent = Agent::with_seams(
            crate::HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(crate::environment::FolderEnvironment::bare())),
                models: tickloop_harness_test_support::harness::ChosenModels::new(Arc::new(
                    tickloop_harness_test_support::ScriptedModel::new(vec![]),
                )),
            },
            options,
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.auto_compact_window, cap);
        let line = crate::context::auto_compact_tokens(&snapshot).unwrap() as u64;
        assert_eq!(
            line,
            tickloop_harness_protocol::auto_compact_line(
                cap.unwrap_or(200_000),
                snapshot.model.max_output_tokens,
                None,
            ) as u64
        );
        assert_eq!(
            agent.status(chat).await.unwrap().auto_compact_tokens,
            Some(line)
        );
        let breakdown = agent.context_breakdown(chat).await.unwrap();
        assert_eq!(breakdown.autocompact_buffer_tokens, Some(200_000 - line));
        assert_eq!(
            breakdown.free_tokens,
            Some(line - breakdown.used_tokens),
            "free space runs out exactly at the compaction line"
        );
        agent.shutdown().await;
    }
}
