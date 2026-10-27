use tickloop_harness::protocol::{
    ContextCategoryUsage, ToolSearchPool, UnavailableToolServer, mcp::McpServerState,
};

use super::*;

fn item(name: &str, tokens: u64) -> ContextItem {
    ContextItem {
        name: name.into(),
        tokens,
    }
}

fn usage(category: ContextCategory, tokens: u64) -> ContextCategoryUsage {
    ContextCategoryUsage { category, tokens }
}

fn breakdown() -> ContextBreakdown {
    ContextBreakdown {
        tokenizer: "o200k_base".into(),
        context_window_tokens: Some(200_000),
        used_tokens: 46_200,
        calibrated: true,
        categories: vec![
            usage(ContextCategory::SystemPrompt, 6_100),
            usage(ContextCategory::SystemTools, 12_400),
            usage(ContextCategory::McpTools, 900),
            usage(ContextCategory::InstructionFiles, 13_500),
            usage(ContextCategory::Skills, 2_300),
            usage(ContextCategory::ToolSearchResults, 1_800),
            usage(ContextCategory::Messages, 9_200),
        ],
        autocompact_buffer_tokens: Some(40_000),
        free_tokens: Some(113_800),
        system_tools: vec![item("Bash", 2_900), item("SearchTools", 400)],
        mcp_tools: vec![ContextToolServer {
            server: "office".into(),
            tools: vec![item("mcp__office__open", 900)],
        }],
        instruction_files: vec![
            item("/home/user/.tickloop/AGENTS.md", 9_000),
            item("/work/project/AGENTS.md", 4_500),
        ],
        skills: vec![item("tickloop", 1_500), item("notes:summarize", 800)],
        tool_search_results: vec![item("SearchTools quote stock", 1_800)],
        messages: vec![item("User messages", 3_000), item("Tool results", 6_200)],
        host_notes: Vec::new(),
        tool_search: Some(ToolSearchPool {
            mode: ToolSearchMode::SearchTools,
            servers: vec![
                ContextToolServer {
                    server: "longbridge".into(),
                    tools: vec![
                        item("mcp__longbridge__quote", 400),
                        item("mcp__longbridge__news", 300),
                    ],
                },
                ContextToolServer {
                    server: "ui5".into(),
                    tools: vec![item("mcp__ui5__create", 250)],
                },
            ],
            unavailable_servers: vec![UnavailableToolServer {
                name: "figma".into(),
                state: McpServerState::NeedsLogin,
            }],
        }),
    }
}

fn render(breakdown: &ContextBreakdown, width: usize, expanded: bool) -> Vec<Line> {
    ContextView {
        breakdown,
        model_name: "Claude Opus",
        model_id: "claude-opus-5-5",
        home: Some(Path::new("/home/user")),
        expanded,
    }
    .lines(width, DisplayProfile::default())
}

fn texts(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
        .collect()
}

/// Rows that begin with two grid cells; a category line has one glyph and then its name.
fn grid_rows(lines: &[String]) -> Vec<&String> {
    let is_cell =
        |character: Option<char>| character.is_some_and(|character| "⛁⛀⛶⛝".contains(character));
    lines
        .iter()
        .filter(|line| {
            let mut characters = line.chars();
            is_cell(characters.next())
                && characters.next() == Some(' ')
                && is_cell(characters.next())
        })
        .collect()
}

#[test]
fn a_wide_terminal_shows_the_grid_beside_the_categories_within_its_width() {
    let lines = texts(&render(&breakdown(), 120, false));
    for line in &lines {
        assert!(line.width() <= 120, "{line}");
    }
    assert_eq!(lines[0], "Context Usage");
    assert!(
        lines[1].starts_with("⛁ ") && lines[1].ends_with("Claude Opus"),
        "{}",
        lines[1]
    );
    assert!(lines[2].ends_with("claude-opus-5-5"));
    assert!(
        lines[3].ends_with("46.2k/200k tokens (23%)"),
        "{}",
        lines[3]
    );
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("⛁ System prompt: 6.1k tokens (3.0%)"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("⛁ Tool search results: 1.8k tokens (0.9%)"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("⛶ Free space: 113.8k (56.9%)"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("⛝ Autocompact buffer: 40k tokens (20.0%)"))
    );
    let rows = grid_rows(&lines);
    assert_eq!(rows.len(), 10);
    assert!(
        rows[8].starts_with("⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝") && rows[9].starts_with("⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝ ⛝"),
        "the buffer's 20% takes the last cells: {rows:?}"
    );
    assert!(
        rows[7].starts_with("⛶ "),
        "free space comes before it: {}",
        rows[7]
    );
    assert!(
        lines.contains(
            &"Estimated with o200k_base · scaled to the context total the service last reported"
                .to_owned()
        )
    );
    assert!(lines.contains(&"└ 2 files · 13.5k tokens".to_owned()));
    assert!(lines.contains(&"└ 2 skills · 2.3k tokens".to_owned()));
    assert!(lines.contains(&"└ 1 tool on 1 server · 900 tokens".to_owned()));
    assert_eq!(lines.last().unwrap(), "/context all to expand");
}

#[test]
fn the_tool_search_pool_lists_servers_apart_from_the_context_in_its_own_colour() {
    let lines = render(&breakdown(), 120, false);
    let heading = lines
        .iter()
        .find(|line| {
            line.first()
                .is_some_and(|(text, _)| text == "Tool search pool")
        })
        .unwrap();
    assert_eq!(heading[0].1, Some(Role::ContextToolSearch));
    let texts = texts(&lines);
    let start = texts
        .iter()
        .position(|line| line.starts_with("Tool search pool"))
        .unwrap();
    assert_eq!(
        &texts[start..start + 6],
        [
            "Tool search pool · SearchTools (loaded on demand, not in context)",
            "└ 3 tools on 2 servers · ~950 tokens if all loaded",
            "  longbridge   2 tools · ~700 tokens",
            "  ui5          1 tool · ~250 tokens",
            "  figma        needs login",
            "SearchTools, DescribeTools and ExecuteTools count under System tools.",
        ]
    );
    let grid_roles = lines
        .iter()
        .flatten()
        .filter(|(text, _)| text.starts_with('⛁') || text.starts_with('⛀'))
        .filter_map(|(_, role)| *role)
        .collect::<Vec<_>>();
    assert!(grid_roles.contains(&Role::ContextToolSearch));
    assert!(grid_roles.contains(&Role::ContextMessages));
}

#[test]
fn a_narrow_terminal_puts_the_categories_under_a_smaller_grid() {
    let lines = texts(&render(&breakdown(), 40, false));
    let rows = grid_rows(&lines);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row.width() <= 10), "{rows:?}");
    let first_category = lines.iter().position(|line| line == "Claude Opus").unwrap();
    assert!(
        first_category
            > lines
                .iter()
                .position(|line| line == rows[4].as_str())
                .unwrap()
    );

    let lines = texts(&render(&breakdown(), 8, false));
    assert!(
        grid_rows(&lines).is_empty(),
        "a grid wider than the terminal is dropped"
    );
}

#[test]
fn without_a_known_window_no_grid_or_shares_are_shown() {
    let mut unknown = breakdown();
    unknown.context_window_tokens = None;
    unknown.free_tokens = None;
    unknown.autocompact_buffer_tokens = None;
    let lines = texts(&render(&unknown, 120, false));
    assert!(grid_rows(&lines).is_empty());
    assert!(lines.contains(&"46.2k tokens · context window unknown".to_owned()));
    assert!(lines.contains(&"⛁ Messages: 9.2k tokens".to_owned()));
    assert!(!lines.iter().any(|line| line.contains("Free space")));
}

#[test]
fn context_all_lists_every_item_the_largest_first_with_home_shortened() {
    let lines = texts(&render(&breakdown(), 120, true));
    let section = |title: &str| {
        let start = lines
            .iter()
            .position(|line| line.starts_with(title))
            .unwrap();
        lines[start + 1..]
            .iter()
            .take_while(|line| !line.is_empty())
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        section("System tools"),
        ["└ Bash: 2.9k tokens", "└ SearchTools: 400 tokens"]
    );
    assert_eq!(
        section("Instruction files"),
        [
            "└ ~/.tickloop/AGENTS.md: 9k tokens",
            "└ /work/project/AGENTS.md: 4.5k tokens"
        ]
    );
    assert_eq!(
        section("MCP tools"),
        [
            "└ 1 tool on 1 server · 900 tokens",
            "  office",
            "  └ mcp__office__open: 900 tokens"
        ]
    );
    assert_eq!(
        section("Messages"),
        ["└ Tool results: 6.2k tokens", "└ User messages: 3k tokens"]
    );
    let pool = section("Tool search pool");
    assert_eq!(pool[2], "    └ mcp__longbridge__quote: ~400 tokens");
    assert!(!lines.iter().any(|line| line.contains("to expand")));
}

#[test]
fn context_all_sums_up_what_a_long_section_leaves_out() {
    let mut long = breakdown();
    long.system_tools = (0..60)
        .map(|index| item(&format!("Tool{index:02}"), 100))
        .collect();
    let lines = texts(&render(&long, 120, true));
    let start = lines
        .iter()
        .position(|line| line == "System tools")
        .unwrap();
    let section = &lines[start + 1..];
    assert_eq!(section[49], "└ Tool49: 100 tokens");
    assert_eq!(section[50], "… 10 more (1k tokens)");
    assert!(section[51].is_empty());
}

#[test]
fn names_from_servers_and_files_cannot_control_the_terminal() {
    let mut hostile = breakdown();
    let tool = item("mcp__evil__\u{1b}[31mred\nnext", 100);
    hostile.mcp_tools = vec![ContextToolServer {
        server: "evil\u{1b}[2J".into(),
        tools: vec![tool.clone()],
    }];
    hostile.tool_search.as_mut().unwrap().servers[0]
        .tools
        .push(tool);
    hostile.tool_search.as_mut().unwrap().unavailable_servers[0].name = "bad\rname".into();
    hostile.instruction_files[0].name = "/work/\u{7}AGENTS.md".into();
    for expanded in [false, true] {
        let lines = texts(&render(&hostile, 120, expanded));
        for line in &lines {
            assert!(
                !line.chars().any(char::is_control),
                "no raw control character reaches the terminal: {line:?}"
            );
        }
        if expanded {
            assert!(lines.contains(&"  └ mcp__evil__\\u{1b}[31mred\\nnext: 100 tokens".to_owned()));
            assert!(lines.contains(&"  evil\\u{1b}[2J".to_owned()));
        }
    }
}

#[test]
fn host_notes_show_apart_from_what_the_user_typed() {
    let mut noted = breakdown();
    noted.messages = vec![item("User messages", 4), item("Assistant messages", 110)];
    noted.host_notes = vec![
        item("Skill guidance · tickloop", 6_000),
        item("MCP server instructions · longbridge\u{1b}[31m", 4_400),
    ];
    let lines = texts(&render(&noted, 120, false));
    assert!(lines.contains(&"└ includes 10.4k tokens of host notes · /context all".to_owned()));
    let lines = texts(&render(&noted, 120, true));
    let start = lines.iter().position(|line| line == "Messages").unwrap();
    assert_eq!(
        &lines[start + 1..start + 5],
        [
            "└ Skill guidance · tickloop: 6k tokens",
            "└ MCP server instructions · longbridge\\u{1b}[31m: 4.4k tokens",
            "└ Assistant messages: 110 tokens",
            "└ User messages: 4 tokens",
        ]
    );
}

#[test]
fn what_the_service_reports_beyond_the_count_has_its_own_row() {
    let mut gap = breakdown();
    gap.categories
        .push(usage(ContextCategory::Unattributed, 12_000));
    let lines = texts(&render(&gap, 120, false));
    assert!(
        lines
            .iter()
            .any(|line| line
                .ends_with("⛁ Unattributed (reported by the service): 12k tokens (6.0%)"))
    );
}
