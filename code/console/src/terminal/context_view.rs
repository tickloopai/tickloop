//! `/context`: the current Chat's context as Claude Code's `/context` shows it. A grid of cells
//! beside the estimated categories, then a section per kind of content, then the MCP tools only
//! tool search reaches, which take no context until loaded. `/context all` lists every item.

use std::path::Path;

use tickloop_harness::protocol::{
    ContextBreakdown, ContextCategory, ContextItem, ContextToolServer, ToolSearchMode,
    mcp::McpServerState,
};
use unicode_width::UnicodeWidthStr;

use super::display::{DisplayProfile, Glyph};
use super::presentation::compact_count;
use super::safe_line;
use super::style::Role;

pub(super) const EXPAND_ARGUMENT: &str = "all";
/// Items a section lists before it sums up the rest.
const MAX_SECTION_ITEMS: usize = 50;
/// Below this width the grid shrinks to five columns, as Claude Code's does.
const NARROW_WIDTH: usize = 80;
const LARGE_WINDOW_TOKENS: u64 = 1_000_000;
/// Each cell is a glyph and a space.
const CELL_COLUMNS: usize = 2;
const COLUMN_GAP: usize = 2;
const FILLED_CELL_SHARE: f64 = 0.7;
/// The widest a pool bar gets, and the terminal width below which pool bars are left out.
const MAX_BAR_CELLS: usize = 24;
const MIN_BAR_CELLS: usize = 6;
const MIN_BAR_WIDTH: usize = 60;
/// Room a pool line keeps beside its name and bar: the indent, gaps, count and tokens.
const POOL_LINE_TEXT_COLUMNS: usize = 30;
const MAX_POOL_NAME_COLUMNS: usize = 20;
const MAX_POOL_TOOL_NAME_COLUMNS: usize = 40;
/// Host notes beyond this many tokens get a line of their own in plain `/context`.
const HOST_NOTES_SUMMARY_TOKENS: u64 = 1_000;

type Run = (String, Option<Role>);
type Line = Vec<Run>;

pub(super) struct ContextView<'a> {
    pub(super) breakdown: &'a ContextBreakdown,
    pub(super) model_name: &'a str,
    pub(super) model_id: &'a str,
    pub(super) home: Option<&'a Path>,
    pub(super) expanded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Cell {
    Full(Role),
    Partial(Role),
    Free,
    Reserved,
}

impl ContextView<'_> {
    /// The view as conversation runs, a block of its own.
    pub(super) fn runs(&self, width: usize, display: DisplayProfile) -> Vec<Run> {
        let mut runs = vec![("\n".to_owned(), None)];
        for line in self.lines(width, display) {
            runs.extend(line);
            runs.push(("\n".into(), None));
        }
        runs
    }

    fn lines(&self, width: usize, display: DisplayProfile) -> Vec<Line> {
        let mut lines = vec![vec![("Context Usage".into(), Some(Role::Strong))]];
        lines.extend(self.overview(width, display));
        lines.extend(self.footnotes());
        lines.extend(self.sections());
        lines.extend(self.tool_search_pool(width, display));
        if !self.expanded {
            lines.push(Vec::new());
            lines.push(muted(format!("/context {EXPAND_ARGUMENT} to expand")));
        }
        // Server, tool, Skill and file names come from outside: none may move the cursor,
        // colour the terminal or start a line of its own.
        for line in &mut lines {
            for (text, _) in line.iter_mut() {
                *text = safe_line(text);
            }
        }
        lines
    }

    /// The grid beside the summary when both fit, the summary below the grid when only the grid
    /// fits, and the summary alone otherwise.
    fn overview(&self, width: usize, display: DisplayProfile) -> Vec<Line> {
        let summary = self.summary(display);
        let Some(window) = self.window() else {
            return summary;
        };
        let narrow = width < NARROW_WIDTH;
        let large = window >= LARGE_WINDOW_TOKENS;
        let columns = if narrow {
            5
        } else if large {
            20
        } else {
            10
        };
        let rows = if large || !narrow { 10 } else { 5 };
        let grid = self
            .cells(columns * rows)
            .chunks(columns)
            .map(|row| cell_line(row, display))
            .collect::<Vec<_>>();
        let grid_width = columns * CELL_COLUMNS;
        let summary_width = summary.iter().map(line_width).max().unwrap_or(0);
        if grid_width + COLUMN_GAP + summary_width <= width {
            let height = grid.len().max(summary.len());
            return (0..height)
                .map(|index| {
                    let mut line = grid
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| vec![(" ".repeat(grid_width), None)]);
                    if let Some(summary) = summary.get(index).filter(|line| !line.is_empty()) {
                        line.push((" ".repeat(COLUMN_GAP), None));
                        line.extend(summary.iter().cloned());
                    }
                    trim_end(line)
                })
                .collect();
        }
        if grid_width <= width {
            let mut lines = grid.into_iter().map(trim_end).collect::<Vec<_>>();
            lines.push(Vec::new());
            lines.extend(summary);
            return lines;
        }
        summary
    }

    fn window(&self) -> Option<u64> {
        self.breakdown
            .context_window_tokens
            .filter(|window| *window > 0)
    }

    fn summary(&self, display: DisplayProfile) -> Vec<Line> {
        let breakdown = self.breakdown;
        let used = compact_count(breakdown.used_tokens);
        let usage = match self.window() {
            Some(window) => format!(
                "{used}/{} tokens ({}%)",
                compact_count(window),
                (breakdown.used_tokens as f64 / window as f64 * 100.0).round()
            ),
            None => format!("{used} tokens · context window unknown"),
        };
        let mut lines = vec![
            vec![(self.model_name.to_owned(), Some(Role::Text))],
            muted(self.model_id.to_owned()),
            muted(usage),
            Vec::new(),
            muted("Estimated usage by category".into()),
        ];
        for usage in &breakdown.categories {
            lines.push(vec![
                (
                    display.glyph(Glyph::ContextFull).into(),
                    Some(category_role(usage.category)),
                ),
                (format!(" {}: ", category_name(usage.category)), None),
                (
                    format!(
                        "{} tokens{}",
                        compact_count(usage.tokens),
                        self.share(usage.tokens)
                    ),
                    Some(Role::MutedText),
                ),
            ]);
        }
        if let Some(free) = breakdown.free_tokens.filter(|tokens| *tokens > 0) {
            lines.push(vec![
                (
                    display.glyph(Glyph::ContextFree).into(),
                    Some(Role::MutedText),
                ),
                (" Free space: ".into(), None),
                (
                    format!("{}{}", compact_count(free), self.share(free)),
                    Some(Role::MutedText),
                ),
            ]);
        }
        if let Some(buffer) = breakdown
            .autocompact_buffer_tokens
            .filter(|tokens| *tokens > 0)
        {
            lines.push(vec![
                (
                    display.glyph(Glyph::ContextReserved).into(),
                    Some(Role::MutedText),
                ),
                (
                    format!(
                        " Autocompact buffer: {} tokens{}",
                        compact_count(buffer),
                        self.share(buffer)
                    ),
                    Some(Role::MutedText),
                ),
            ]);
        }
        lines
    }

    /// ` (12.5%)` of the window, when the window is known.
    fn share(&self, tokens: u64) -> String {
        self.window().map_or_else(String::new, |window| {
            format!(" ({:.1}%)", tokens as f64 / window as f64 * 100.0)
        })
    }

    /// Claude Code's grid: each category takes at least one cell in order, free space fills the
    /// middle and the autocompact buffer takes the last cells.
    fn cells(&self, total: usize) -> Vec<Cell> {
        let Some(window) = self.window() else {
            return Vec::new();
        };
        let exact = |tokens: u64| tokens as f64 / window as f64 * total as f64;
        let category_cells = |tokens: u64, role: Role| {
            let exact = exact(tokens);
            let whole = exact.floor() as usize;
            let fraction = exact - exact.floor();
            (0..(exact.round() as usize).max(1))
                .map(|index| {
                    if index == whole && fraction > 0.0 && fraction < FILLED_CELL_SHARE {
                        Cell::Partial(role)
                    } else {
                        Cell::Full(role)
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut cells = self
            .breakdown
            .categories
            .iter()
            .flat_map(|usage| category_cells(usage.tokens, category_role(usage.category)))
            .take(total)
            .collect::<Vec<_>>();
        let reserved = self
            .breakdown
            .autocompact_buffer_tokens
            .filter(|tokens| *tokens > 0)
            .map_or(0, |tokens| (exact(tokens).round() as usize).max(1));
        let free_end = total.saturating_sub(reserved).max(cells.len());
        cells.resize(free_end, Cell::Free);
        cells.resize(total, Cell::Reserved);
        cells
    }

    fn footnotes(&self) -> Vec<Line> {
        let breakdown = self.breakdown;
        let mut lines = vec![Vec::new()];
        if let Some(window) = self.window() {
            lines.push(muted(format!(
                "Auto-compact window: {} tokens",
                compact_count(window)
            )));
        }
        let basis = if breakdown.calibrated {
            "scaled to the context total the service last reported"
        } else {
            "no request has measured this context yet"
        };
        lines.push(muted(format!(
            "Estimated with {} · {basis}",
            breakdown.tokenizer
        )));
        lines
    }

    fn sections(&self) -> Vec<Line> {
        let breakdown = self.breakdown;
        let category_tokens = |category: ContextCategory| {
            breakdown
                .categories
                .iter()
                .find(|usage| usage.category == category)
                .map_or(0, |usage| usage.tokens)
        };
        let mut lines = Vec::new();
        if self.expanded && !breakdown.system_tools.is_empty() {
            lines.extend(self.item_section(
                "System tools",
                "",
                "tool",
                &breakdown.system_tools,
                category_tokens(ContextCategory::SystemTools),
            ));
        }
        if !breakdown.mcp_tools.is_empty() {
            lines.extend(self.server_section(&breakdown.mcp_tools));
        }
        let files = breakdown
            .instruction_files
            .iter()
            .map(|file| ContextItem {
                name: self.display_path(&file.name),
                tokens: file.tokens,
            })
            .collect::<Vec<_>>();
        if !files.is_empty() {
            lines.extend(self.item_section(
                "Instruction files",
                "",
                "file",
                &files,
                category_tokens(ContextCategory::InstructionFiles),
            ));
        }
        if !breakdown.skills.is_empty() {
            lines.extend(self.item_section(
                "Skills",
                "",
                "skill",
                &breakdown.skills,
                category_tokens(ContextCategory::Skills),
            ));
        }
        if !breakdown.tool_search_results.is_empty() {
            lines.extend(self.item_section(
                "Tool search results",
                "",
                "result",
                &breakdown.tool_search_results,
                category_tokens(ContextCategory::ToolSearchResults),
            ));
        }
        let host_notes = breakdown
            .host_notes
            .iter()
            .map(|note| note.tokens)
            .sum::<u64>();
        if self.expanded {
            let mut items = breakdown
                .messages
                .iter()
                .chain(&breakdown.host_notes)
                .cloned()
                .collect::<Vec<_>>();
            items.sort_by_key(|item| std::cmp::Reverse(item.tokens));
            if !items.is_empty() {
                lines.push(Vec::new());
                lines.push(heading("Messages", ""));
                lines.extend(item_lines(&items, "", ""));
            }
        } else if host_notes >= HOST_NOTES_SUMMARY_TOKENS {
            lines.push(Vec::new());
            lines.push(heading("Messages", ""));
            lines.push(tree_line(
                "",
                format!(
                    "includes {} tokens of host notes · /context {EXPAND_ARGUMENT}",
                    compact_count(host_notes)
                ),
            ));
        }
        lines
    }

    /// A section's heading and count; expanded, its items, the largest first.
    fn item_section(
        &self,
        title: &str,
        hint: &str,
        noun: &str,
        items: &[ContextItem],
        tokens: u64,
    ) -> Vec<Line> {
        let mut lines = vec![Vec::new(), heading(title, hint)];
        if self.expanded {
            lines.extend(item_lines(items, "", ""));
        } else {
            lines.push(tree_line(
                "",
                format!(
                    "{} {} · {} tokens",
                    items.len(),
                    plural(items.len(), noun),
                    compact_count(tokens)
                ),
            ));
        }
        lines
    }

    fn server_section(&self, servers: &[ContextToolServer]) -> Vec<Line> {
        let tools = servers
            .iter()
            .map(|server| server.tools.len())
            .sum::<usize>();
        let tokens = servers.iter().map(ContextToolServer::tokens).sum::<u64>();
        let mut lines = vec![
            Vec::new(),
            heading("MCP tools", " · /mcp (sent with every request)"),
            tree_line(
                "",
                format!(
                    "{tools} {} on {} {} · {} tokens",
                    plural(tools, "tool"),
                    servers.len(),
                    plural(servers.len(), "server"),
                    compact_count(tokens)
                ),
            ),
        ];
        if self.expanded {
            for server in servers {
                lines.push(muted(format!("  {}", server.server)));
                lines.extend(item_lines(&server.tools, "  ", ""));
            }
        }
        lines
    }

    /// The MCP tools only tool search reaches, by server: independent of the context above. A bar
    /// shows each server's share of the pool, and in `/context all` each tool's size against the
    /// largest one.
    fn tool_search_pool(&self, width: usize, display: DisplayProfile) -> Vec<Line> {
        let Some(pool) = &self.breakdown.tool_search else {
            return Vec::new();
        };
        let mode = match pool.mode {
            ToolSearchMode::Native => "native",
            ToolSearchMode::SearchTools => "SearchTools",
        };
        let servers = pool.servers.len();
        let mut lines = vec![
            Vec::new(),
            vec![
                ("Tool search pool".into(), Some(Role::ContextToolSearch)),
                (
                    format!(" · {mode} (loaded on demand, not in context)"),
                    Some(Role::MutedText),
                ),
            ],
            tree_line(
                "",
                format!(
                    "{} tools on {servers} {} · ~{} tokens if all loaded",
                    pool.tool_count(),
                    plural(servers, "server"),
                    compact_count(pool.tokens())
                ),
            ),
        ];
        let names = pool
            .servers
            .iter()
            .map(|server| server.server.as_str())
            .chain(
                pool.unavailable_servers
                    .iter()
                    .map(|server| server.name.as_str()),
            )
            .map(|name| clipped_name(name, MAX_POOL_NAME_COLUMNS))
            .collect::<Vec<_>>();
        let name_width = names.iter().map(|name| name.width()).max().unwrap_or(0);
        let count_width = pool
            .servers
            .iter()
            .map(|server| server.tools.len().to_string().len())
            .max()
            .unwrap_or(1);
        let bar_cells = bar_cells(width, name_width);
        let pool_tokens = pool.tokens();
        let largest_tool = pool
            .servers
            .iter()
            .flat_map(|server| &server.tools)
            .map(|tool| tool.tokens)
            .max()
            .unwrap_or(0);
        let padded = |name: &str| {
            format!(
                "  {name}{}  ",
                " ".repeat(name_width.saturating_sub(name.width()))
            )
        };
        for (server, name) in pool.servers.iter().zip(&names) {
            let tools = server.tools.len();
            let mut line = vec![(padded(name), None)];
            line.extend(bar(server.tokens(), pool_tokens, bar_cells, display));
            line.push((
                format!(
                    "{tools:>count_width$} {} · ~{}",
                    plural(tools, "tool"),
                    compact_count(server.tokens())
                ),
                Some(Role::MutedText),
            ));
            lines.push(line);
            if self.expanded {
                lines.extend(self.pool_tool_lines(&server.tools, largest_tool, bar_cells, display));
            }
        }
        for (server, name) in pool
            .unavailable_servers
            .iter()
            .zip(&names[pool.servers.len()..])
        {
            lines.push(vec![
                (padded(name), None),
                (server_state(&server.state).into(), Some(Role::MutedText)),
            ]);
        }
        if pool.mode == ToolSearchMode::SearchTools {
            lines.push(muted(
                "SearchTools, DescribeTools and ExecuteTools count under System tools.".into(),
            ));
        }
        lines
    }

    /// A server's tools, the largest first, each with a bar against the pool's largest tool.
    fn pool_tool_lines(
        &self,
        tools: &[ContextItem],
        largest: u64,
        bar_cells: usize,
        display: DisplayProfile,
    ) -> Vec<Line> {
        let shown = &tools[..tools.len().min(MAX_SECTION_ITEMS)];
        let names = shown
            .iter()
            .map(|tool| clipped_name(&tool.name, MAX_POOL_TOOL_NAME_COLUMNS))
            .collect::<Vec<_>>();
        let name_width = names.iter().map(|name| name.width()).max().unwrap_or(0);
        let mut lines = shown
            .iter()
            .zip(names)
            .map(|(tool, name)| {
                let mut line = vec![(
                    format!(
                        "    └ {name}{}  ",
                        " ".repeat(name_width.saturating_sub(name.width()))
                    ),
                    None,
                )];
                line.extend(bar(tool.tokens, largest, bar_cells, display));
                line.push((
                    format!("~{} tokens", compact_count(tool.tokens)),
                    Some(Role::MutedText),
                ));
                line
            })
            .collect::<Vec<_>>();
        let rest = &tools[shown.len()..];
        if !rest.is_empty() {
            lines.push(muted(format!(
                "    … {} more (~{} tokens)",
                rest.len(),
                compact_count(rest.iter().map(|tool| tool.tokens).sum())
            )));
        }
        lines
    }

    fn display_path(&self, path: &str) -> String {
        self.home
            .and_then(|home| Path::new(path).strip_prefix(home).ok())
            .map_or_else(
                || path.to_owned(),
                |relative| format!("~/{}", relative.display()),
            )
    }
}

fn category_name(category: ContextCategory) -> &'static str {
    match category {
        ContextCategory::SystemPrompt => "System prompt",
        ContextCategory::SystemTools => "System tools",
        ContextCategory::McpTools => "MCP tools",
        ContextCategory::InstructionFiles => "Instruction files",
        ContextCategory::Skills => "Skills",
        ContextCategory::ToolSearchResults => "Tool search results",
        ContextCategory::Messages => "Messages",
        ContextCategory::Unattributed => "Unattributed (reported by the service)",
    }
}

/// Claude Code's `/context` colours, by category.
fn category_role(category: ContextCategory) -> Role {
    match category {
        ContextCategory::SystemPrompt => Role::PromptBorder,
        ContextCategory::SystemTools => Role::MutedText,
        ContextCategory::McpTools => Role::ContextMcpTools,
        ContextCategory::InstructionFiles => Role::Accent,
        ContextCategory::Skills => Role::Warning,
        ContextCategory::ToolSearchResults => Role::ContextToolSearch,
        ContextCategory::Messages => Role::ContextMessages,
        ContextCategory::Unattributed => Role::Subtle,
    }
}

fn server_state(state: &McpServerState) -> &'static str {
    match state {
        McpServerState::Connecting => "connecting",
        McpServerState::Connected => "connected",
        McpServerState::Failed => "failed",
        McpServerState::Unsupported => "unsupported",
        McpServerState::NeedsLogin => "needs login",
        McpServerState::NeedsConfiguration => "needs configuration",
        McpServerState::Disabled => "disabled",
    }
}

fn cell_line(cells: &[Cell], display: DisplayProfile) -> Line {
    cells
        .iter()
        .map(|cell| {
            let (glyph, role) = match cell {
                Cell::Full(role) => (Glyph::ContextFull, *role),
                Cell::Partial(role) => (Glyph::ContextPartial, *role),
                Cell::Free => (Glyph::ContextFree, Role::MutedText),
                Cell::Reserved => (Glyph::ContextReserved, Role::MutedText),
            };
            (format!("{} ", display.glyph(glyph)), Some(role))
        })
        .collect()
}

/// Items, at most [`MAX_SECTION_ITEMS`] of them, then what the rest add up to.
fn item_lines(items: &[ContextItem], indent: &str, approximate: &str) -> Vec<Line> {
    let mut lines = items
        .iter()
        .take(MAX_SECTION_ITEMS)
        .map(|item| {
            vec![
                (format!("{indent}└ {}: ", item.name), None),
                (
                    format!("{approximate}{} tokens", compact_count(item.tokens)),
                    Some(Role::MutedText),
                ),
            ]
        })
        .collect::<Vec<_>>();
    let rest = &items[items.len().min(MAX_SECTION_ITEMS)..];
    if !rest.is_empty() {
        lines.push(muted(format!(
            "{indent}… {} more ({approximate}{} tokens)",
            rest.len(),
            compact_count(rest.iter().map(|item| item.tokens).sum())
        )));
    }
    lines
}

/// How many cells a pool bar takes at `width`: none on a narrow terminal.
fn bar_cells(width: usize, name_width: usize) -> usize {
    if width < MIN_BAR_WIDTH {
        return 0;
    }
    let cells = width
        .saturating_sub(name_width + POOL_LINE_TEXT_COLUMNS)
        .min(MAX_BAR_CELLS);
    if cells < MIN_BAR_CELLS { 0 } else { cells }
}

/// `part` of `whole` as filled cells out of `cells`, then a gap; any nonzero part fills one.
fn bar(part: u64, whole: u64, cells: usize, display: DisplayProfile) -> Vec<Run> {
    if cells == 0 {
        return Vec::new();
    }
    let filled = if whole == 0 || part == 0 {
        0
    } else {
        ((part as f64 / whole as f64 * cells as f64).round() as usize).clamp(1, cells)
    };
    vec![
        (
            display.glyph(Glyph::BarFilled).repeat(filled),
            Some(Role::ContextToolSearch),
        ),
        (
            display.glyph(Glyph::BarEmpty).repeat(cells - filled),
            Some(Role::MutedText),
        ),
        ("  ".into(), None),
    ]
}

/// A name cut to `columns`, ending in `…` when cut.
fn clipped_name(name: &str, columns: usize) -> String {
    let name = safe_line(name);
    if name.width() <= columns {
        return name;
    }
    let mut clipped = super::console::truncate_width(&name, columns.saturating_sub(1));
    clipped.push('…');
    clipped
}

fn heading(title: &str, hint: &str) -> Line {
    let mut line = vec![(title.to_owned(), Some(Role::Strong))];
    if !hint.is_empty() {
        line.push((hint.to_owned(), Some(Role::MutedText)));
    }
    line
}

fn tree_line(indent: &str, text: String) -> Line {
    vec![(format!("{indent}└ "), None), (text, Some(Role::MutedText))]
}

fn muted(text: String) -> Line {
    vec![(text, Some(Role::MutedText))]
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

fn line_width(line: &Line) -> usize {
    line.iter().map(|(text, _)| text.width()).sum()
}

/// Drops the padding a grid row ends with, which would otherwise wrap a narrow terminal.
fn trim_end(mut line: Line) -> Line {
    while let Some((text, _)) = line.last_mut() {
        let trimmed = text.trim_end().len();
        if trimmed == 0 {
            line.pop();
        } else {
            text.truncate(trimmed);
            break;
        }
    }
    line
}

#[cfg(test)]
#[path = "context_view_tests.rs"]
mod tests;
