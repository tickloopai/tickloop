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

    fn protocol_data_tokens(&self, protocol: Protocol, value: &Value) -> usize {
        context_tokens::protocol_record_tokens(self, protocol, value)
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
        message_usage: vec![None; messages.len()],
        protocol: Protocol::Responses,
        prior_token_ratio: None,
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

/// Historical guidance remains attributable even after the built-in Skill is retired.
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

fn words(count: usize) -> String {
    vec!["w"; count].join(" ")
}

fn response_usage(output: u64, reasoning: Option<u64>) -> Option<Usage> {
    Some(Usage {
        output_tokens: Some(output),
        reasoning_output_tokens: reasoning,
        ..Usage::default()
    })
}

fn with_usage(mut parts: ContextParts, usage: Vec<(usize, Option<Usage>)>) -> ContextParts {
    for (index, usage) in usage {
        parts.message_usage[index] = usage;
    }
    parts
}

#[test]
fn sealed_reasoning_counts_as_the_reasoning_its_response_reported_never_as_its_payload() {
    let reasoning = Content::ProtocolData {
        protocol: Protocol::Responses,
        value: json!({"type": "reasoning", "encrypted_content": words(500), "summary": []}),
    };
    let messages = vec![
        message(Role::User, vec![text("go")]),
        message(Role::Assistant, vec![reasoning, text("done here")]),
    ];
    let assistant = |usage: Option<Usage>| {
        let breakdown =
            with_usage(parts(messages.clone(), None), vec![(1, usage)]).count(&WordCounter);
        item_tokens(&breakdown.messages, ASSISTANT_MESSAGES)
    };
    assert_eq!(assistant(response_usage(42, Some(40))), 3 + 2 + 40);
    assert_eq!(
        assistant(response_usage(42, None)),
        3 + 2 + 40,
        "usage recorded before reasoning was reported: the output beside the visible part"
    );
    assert_eq!(
        assistant(None),
        3 + 2,
        "another model's response: only readable reasoning counts"
    );
}

#[test]
fn tickloops_own_record_fields_never_count() {
    let origin = words(30);
    let messages = vec![message(
        Role::Assistant,
        vec![
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value: json!({
                    "type": "_tickloop_assistant_phase",
                    "phase": "commentary",
                    "_tickloop_codex_origin": origin,
                }),
            },
            text("checking now"),
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value: json!({"type": "record", "detail": "kept", "_tickloop_codex_origin": origin}),
            },
        ],
    )];
    let breakdown = parts(messages, None).count(&WordCounter);
    assert_eq!(
        item_tokens(&breakdown.messages, ASSISTANT_MESSAGES),
        3 + 1 + 2 + 1,
        "the phase as sent, the text and the record without the origin"
    );
}

#[test]
fn omitted_thinking_counts_as_the_thinking_its_response_reported() {
    let messages = vec![
        message(Role::User, vec![text("read it")]),
        message(
            Role::Assistant,
            vec![
                Content::Thinking {
                    thinking: String::new(),
                    signature: Some("sealed".into()),
                },
                Content::ToolUse {
                    id: "call".into(),
                    name: "Read".into(),
                    input: json!("a b"),
                },
            ],
        ),
    ];
    let assistant = |usage: Option<Usage>| {
        let mut claude = with_usage(parts(messages.clone(), None), vec![(1, usage)]);
        claude.protocol = Protocol::Messages;
        claude.prior_token_ratio = Some(1.5);
        item_tokens(&claude.count(&WordCounter).messages, ASSISTANT_MESSAGES)
    };
    assert_eq!(
        assistant(response_usage(333, Some(300))),
        3 + 3 + 200,
        "the service's 300 thinking tokens at its 1.5 tokens per counted token"
    );
    assert_eq!(assistant(response_usage(333, None)), 3 + 3 + (222 - 3));
}

/// A conversation of `rounds` exchanges whose service counts `ratio` tokens for each counted
/// one: every request's input is the scaled count of what it sends, every response's output the
/// scaled count of the message it wrote.
fn measured_conversation(rounds: usize, user_words: usize, ratio: f64) -> ContextParts {
    let scaled = |tokens: u64| (tokens as f64 * ratio).round() as u64;
    let mut messages = Vec::new();
    let mut usage = Vec::new();
    for round in 0..rounds {
        messages.push(message(Role::User, vec![text(&words(user_words))]));
        let input = scaled(
            parts(messages.clone(), None)
                .count(&WordCounter)
                .used_tokens,
        );
        let answer = message(Role::Assistant, vec![text(&words(20 + round))]);
        let output = scaled(WordCounter.message_tokens(&answer) as u64);
        messages.push(answer);
        usage.push((
            messages.len() - 1,
            Some(Usage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                ..Usage::default()
            }),
        ));
    }
    let (last_input, last_output) = usage
        .last()
        .and_then(|(_, usage)| usage.as_ref())
        .map(|usage| (usage.input_tokens.unwrap(), usage.output_tokens.unwrap()))
        .unwrap();
    let mut parts = with_usage(parts(messages, None), usage);
    parts.window = Some(100_000);
    parts.compaction_trigger = None;
    parts.reported_total = Some(last_input + last_output);
    parts
}

#[test]
fn a_service_tokenizer_measured_from_consecutive_responses_scales_every_category() {
    let conversation = measured_conversation(5, 1_500, 1.56);
    let counted = {
        let mut uncalibrated = measured_conversation(5, 1_500, 1.56);
        uncalibrated.reported_total = None;
        uncalibrated.count(&WordCounter)
    };
    let reported = conversation.reported_total.unwrap();
    assert!(
        reported > counted.used_tokens * 150 / 100,
        "beyond the 15% the count alone explains"
    );
    let breakdown = conversation.count(&WordCounter);
    assert_eq!(breakdown.used_tokens, reported);
    assert_eq!(category_sum(&breakdown), reported);
    assert_eq!(category(&breakdown, ContextCategory::Unattributed), 0);
    for category_name in [ContextCategory::SystemPrompt, ContextCategory::Messages] {
        let scaled = category(&counted, category_name) as f64 * 1.56;
        let calibrated = category(&breakdown, category_name) as f64;
        assert!(
            (calibrated - scaled).abs() <= scaled * 0.01 + 1.0,
            "{category_name:?}: {calibrated} against {scaled}"
        );
    }
}

#[test]
fn a_tokenizer_matching_the_count_keeps_the_calibration_it_had() {
    let conversation = measured_conversation(5, 1_500, 1.0);
    let reported = conversation.reported_total.unwrap();
    let breakdown = conversation.count(&WordCounter);
    let mut uncalibrated = measured_conversation(5, 1_500, 1.0);
    uncalibrated.reported_total = None;
    let counted = uncalibrated.count(&WordCounter);
    assert_eq!(breakdown.used_tokens, reported);
    assert_eq!(category(&breakdown, ContextCategory::Unattributed), 0);
    assert_eq!(breakdown.categories, counted.categories, "{reported}");
}

#[test]
fn what_the_measured_ratio_cannot_explain_still_stands_apart() {
    let mut conversation = measured_conversation(5, 1_500, 1.0);
    let reported = conversation.reported_total.unwrap() + 3_000;
    conversation.reported_total = Some(reported);
    conversation.prior_token_ratio = Some(1.6);
    let breakdown = conversation.count(&WordCounter);
    assert_eq!(breakdown.used_tokens, reported);
    assert_eq!(
        category(&breakdown, ContextCategory::Unattributed),
        3_000,
        "the Chat measured ratio one, outweighing the catalog's starting ratio"
    );
}

#[test]
fn before_a_chat_measures_its_ratio_the_catalogs_explains_its_first_request() {
    let mut first = parts(vec![message(Role::User, vec![text(&words(200))])], None);
    let counted = first.count_uncalibrated();
    first.prior_token_ratio = Some(1.6);
    first.reported_total = Some(counted * 17 / 10);
    let breakdown = first.count(&WordCounter);
    assert_eq!(category(&breakdown, ContextCategory::Unattributed), 0);
    assert_eq!(category_sum(&breakdown), counted * 17 / 10);
}

impl ContextParts {
    fn count_uncalibrated(&self) -> u64 {
        ContextParts {
            system_prompt: self.system_prompt.clone(),
            system_tools: self.system_tools.clone(),
            skill_tool: self.skill_tool.clone(),
            skills: self.skills.clone(),
            mcp_tools: self.mcp_tools.clone(),
            user_context: self.user_context.clone(),
            instruction_files: self.instruction_files.clone(),
            messages: self.messages.clone(),
            message_labels: self.message_labels.clone(),
            message_usage: self.message_usage.clone(),
            protocol: self.protocol,
            prior_token_ratio: self.prior_token_ratio,
            search: None,
            window: self.window,
            compaction_trigger: self.compaction_trigger,
            reported_total: None,
        }
        .count(&WordCounter)
        .used_tokens
    }
}

/// Token counts of a real Chat, with every block's text replaced by as many words as
/// `o200k_base` counted for it and the usage each response reported, made with
/// `/context` regression in mind.
#[derive(serde::Deserialize)]
struct CalibrationFixture {
    source: String,
    protocol: Protocol,
    system_prompt_tokens: usize,
    reported_total: u64,
    messages: Vec<FixtureMessage>,
}

#[derive(serde::Deserialize)]
struct FixtureMessage {
    #[serde(rename = "r")]
    role: String,
    #[serde(rename = "b")]
    blocks: Vec<(String, usize)>,
    /// Input, output, cache read and cache write tokens of the response of the Chat's model.
    #[serde(rename = "u", default)]
    usage: Option<[Option<u64>; 4]>,
}

fn fixture_parts(fixture: &str) -> (ContextParts, String) {
    let fixture: CalibrationFixture = serde_json::from_str(fixture).unwrap();
    let block = |kind: &str, tokens: usize| match kind {
        "text" => text(&words(tokens)),
        "tool_use" => Content::ToolUse {
            id: "call".into(),
            name: "Tool".into(),
            input: json!(words(tokens.saturating_sub(1))),
        },
        "tool_result" => Content::ToolResult {
            tool_use_id: "call".into(),
            content: tickloop_harness_protocol::ToolResultContent::Text(words(tokens)),
            is_error: false,
        },
        "thinking" => Content::Thinking {
            thinking: words(tokens),
            signature: Some("sealed".into()),
        },
        "reasoning" => Content::ProtocolData {
            protocol: Protocol::Responses,
            value: json!({"type": "reasoning", "encrypted_content": words(tokens)}),
        },
        _ => Content::ProtocolData {
            protocol: fixture.protocol,
            value: json!({"type": "record", "text": words(tokens)}),
        },
    };
    let messages = fixture
        .messages
        .iter()
        .map(|entry| {
            message(
                if entry.role == "a" {
                    Role::Assistant
                } else {
                    Role::User
                },
                entry
                    .blocks
                    .iter()
                    .map(|(kind, tokens)| block(kind, *tokens))
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    let parts = ContextParts {
        system_prompt: message(
            Role::System,
            vec![text(&words(fixture.system_prompt_tokens - 3))],
        ),
        system_tools: Vec::new(),
        skill_tool: None,
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        user_context: message(Role::User, Vec::new()),
        instruction_files: Vec::new(),
        message_labels: vec![None; messages.len()],
        message_usage: fixture
            .messages
            .iter()
            .map(|entry| {
                entry
                    .usage
                    .map(|[input, output, cache_read, cache_write]| Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_input_tokens: cache_read,
                        cache_write_input_tokens: cache_write,
                        reasoning_output_tokens: None,
                    })
            })
            .collect(),
        messages,
        protocol: fixture.protocol,
        prior_token_ratio: (fixture.protocol == Protocol::Messages).then_some(1.6),
        search: None,
        window: Some(1_000_000),
        compaction_trigger: None,
        reported_total: Some(fixture.reported_total),
    };
    (parts, fixture.source)
}

#[test]
fn real_claude_chats_leave_no_large_share_unitemized() {
    for fixture in [
        include_str!("../tests/fixtures/context-calibration/opus-5-5-long.json"),
        include_str!("../tests/fixtures/context-calibration/opus-5-5-first-request.json"),
    ] {
        let (parts, source) = fixture_parts(fixture);
        let reported = parts.reported_total.unwrap();
        let counted = parts.count_uncalibrated();
        assert!(
            reported > counted * 115 / 100,
            "{source}: the count alone leaves {} unexplained",
            reported - counted
        );
        let breakdown = parts.count(&WordCounter);
        assert_eq!(breakdown.used_tokens, reported, "{source}");
        assert_eq!(
            category(&breakdown, ContextCategory::Unattributed),
            0,
            "{source}"
        );
    }
}

#[test]
fn a_real_sol_chat_keeps_its_total_and_counts_reasoning_by_its_tokens() {
    let (parts, source) = fixture_parts(include_str!(
        "../tests/fixtures/context-calibration/gpt-6-1-sol.json"
    ));
    let reported = parts.reported_total.unwrap();
    let ratio = parts.token_ratio(&WordCounter);
    assert!((0.98..=1.05).contains(&ratio), "{source}: ratio {ratio}");
    let breakdown = parts.count(&WordCounter);
    assert_eq!(breakdown.used_tokens, reported, "{source}");
    assert_eq!(category_sum(&breakdown), reported, "{source}");
    assert_eq!(
        category(&breakdown, ContextCategory::Unattributed),
        0,
        "{source}"
    );
}

/// Omitted thinking counts by the usage its response recorded, and only for responses of the
/// model the Chat uses now: another model's reasoning is not what this one reads.
#[tokio::test]
async fn a_chats_responses_count_their_reasoning_by_their_recorded_usage() {
    use tickloop_harness_model::ModelConfigExt;
    let (agent, chat_id, _directory) = agent_with_catalog().await;
    let mut chats = agent.inner.chats.lock().await;
    let chat = chats.get_mut(&chat_id).unwrap();
    let current = chat.snapshot.model.identity();
    let mut previous = current.clone();
    previous.requested_model = "earlier-model".into();
    for (model, answer) in [(previous, "first answer"), (current, "second answer")] {
        push(chat, Role::User, "question", None);
        let index = chat.snapshot.messages.len();
        chat.snapshot.messages.push(message(
            Role::Assistant,
            vec![
                Content::Thinking {
                    thinking: String::new(),
                    signature: Some("sealed".into()),
                },
                text(answer),
            ],
        ));
        chat.snapshot
            .message_ids
            .insert(index, tickloop_ids::generate());
        chat.snapshot.message_models.insert(index, model);
        if let Some(source) = chat.snapshot.model.private_history_source() {
            chat.snapshot.message_sources.insert(index, source);
        }
        chat.snapshot
            .message_usage
            .insert(index, response_usage(52, Some(50)).unwrap());
    }
    let chat = chats.get(&chat_id).unwrap();
    let breakdown = agent
        .context_parts(chat, &[], "2026-10-28")
        .unwrap()
        .count(&WordCounter);
    drop(chats);
    assert_eq!(
        item_tokens(&breakdown.messages, ASSISTANT_MESSAGES),
        (3 + 2) + (3 + 2 + 50)
    );
    agent.shutdown().await;
}

#[test]
fn an_uncalibrated_count_stands_whole_whatever_ratio_the_chat_measured() {
    let mut conversation = measured_conversation(5, 1_500, 0.6);
    conversation.reported_total = None;
    let breakdown = conversation.count(&WordCounter);
    assert!(!breakdown.calibrated);
    assert_eq!(category(&breakdown, ContextCategory::Unattributed), 0);
    assert_eq!(category_sum(&breakdown), breakdown.used_tokens);
}
