//! Claude Code's `Glob` and `Grep` tools (`GlobTool.ts`, `utils/glob.ts`, `GrepTool.ts`) on
//! ripgrep's own libraries: `ignore` walks the tree the way `rg --files` does, and
//! `grep-searcher` with `grep-printer` produce the lines `rg` would print.

use std::{
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use grep_printer::{StandardBuilder, SummaryBuilder, SummaryKind};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use ignore::{WalkBuilder, overrides::OverrideBuilder, types::TypesBuilder};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    Operation, READ_TOOL_NAME, ToolDefinition, ToolError, io_error, is_sensitive_path,
    open_existing,
    read::semantic_integer,
    shell::BASH_TOOL_NAME,
    write::{FILE_NOT_FOUND_CWD_NOTE, semantic_boolean},
};
use tickloop_agent_protocol::ToolResult;

pub const GLOB_TOOL_NAME: &str = "Glob";
pub const GREP_TOOL_NAME: &str = "Grep";

/// Claude Code's `globLimits.maxResults` default.
const MAX_GLOB_RESULTS: usize = 100;
const GLOB_TRUNCATED_NOTE: &str =
    "(Results are truncated. Consider using a more specific path or pattern.)";
/// Claude Code `DEFAULT_HEAD_LIMIT`: the entries Grep returns when `head_limit` is unspecified.
const DEFAULT_HEAD_LIMIT: usize = 250;
/// Claude Code passes `--max-columns 500` so minified or base64 lines do not flood the output.
const MAX_COLUMNS: u64 = 500;
const MAX_SEARCH_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Output collected from the files searched so far; like ripgrep's output buffer in Claude Code,
/// reaching it ends the search with the lines already found.
const MAX_SEARCH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const CURRENT_DIRECTORY: &str = ".";
const CONTEXT_SEPARATOR: &[u8] = b"--";
const RIPGREP_IGNORE_FILE: &str = ".rgignore";
/// Glob lists ignored and hidden files like `rg --files --no-ignore --hidden`, but never this
/// project's private data or Git's object store.
const GLOB_SKIPPED_NAMES: &[&str] = &[".git", ".tickloop"];
/// Claude Code `VCS_DIRECTORIES_TO_EXCLUDE`, plus this project's private data.
const GREP_SKIPPED_NAMES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl", ".tickloop"];

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: GLOB_TOOL_NAME.into(),
            description: "- Fast file pattern matching tool that works with any codebase size
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"
- Returns matching file paths sorted by modification time
- Use this tool when you need to find files by name patterns"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The glob pattern to match files against"
                    },
                    "path": {
                        "type": "string",
                        "description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: GREP_TOOL_NAME.into(),
            description: format!(
                r#"A powerful search tool built on ripgrep

  Usage:
  - ALWAYS use {GREP_TOOL_NAME} for search tasks. NEVER invoke `grep` or `rg` as a {BASH_TOOL_NAME} command. The {GREP_TOOL_NAME} tool has been optimized for correct permissions and access.
  - Supports full regex syntax (e.g., "log.*Error", "function\s+\w+")
  - Filter files with glob parameter (e.g., "*.js", "**/*.tsx") or type parameter (e.g., "js", "py", "rust")
  - Output modes: "content" shows matching lines, "files_with_matches" shows only file paths (default), "count" shows match counts
  - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping (use `interface\{{\}}` to find `interface{{}}` in Go code)
  - Multiline matching: By default patterns match within single lines only. For cross-line patterns like `struct \{{[\s\S]*?field`, use `multiline: true`"#
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The regular expression pattern to search for in file contents"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search in (rg PATH). Defaults to current working directory."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob"
                    },
                    "output_mode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "Output mode: \"content\" shows matching lines (supports -A/-B/-C context, -n line numbers, head_limit), \"files_with_matches\" shows file paths (supports head_limit), \"count\" shows match counts (supports head_limit). Defaults to \"files_with_matches\"."
                    },
                    "-B": {
                        "type": "integer",
                        "description": "Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise."
                    },
                    "-A": {
                        "type": "integer",
                        "description": "Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise."
                    },
                    "-C": {
                        "type": "integer",
                        "description": "Alias for context."
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of lines to show before and after each match (rg -C). Requires output_mode: \"content\", ignored otherwise."
                    },
                    "-n": {
                        "type": "boolean",
                        "description": "Show line numbers in output (rg -n). Requires output_mode: \"content\", ignored otherwise. Defaults to true."
                    },
                    "-i": {
                        "type": "boolean",
                        "description": "Case insensitive search (rg -i)"
                    },
                    "type": {
                        "type": "string",
                        "description": "File type to search (rg --type). Common types: js, py, rust, go, java, etc. More efficient than include for standard file types."
                    },
                    "head_limit": {
                        "type": "integer",
                        "description": "Limit output to first N lines/entries, equivalent to \"| head -N\". Works across all output modes: content (limits output lines), files_with_matches (limits file paths), count (limits count entries). Defaults to 250 when unspecified. Pass 0 for unlimited (use sparingly \u{2014} large result sets waste context)."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Skip first N lines/entries before applying head_limit, equivalent to \"| tail -n +N | head -N\". Works across all output modes. Defaults to 0."
                    },
                    "multiline": {
                        "type": "boolean",
                        "description": "Enable multiline mode where . matches newlines and patterns can span lines (rg -U --multiline-dotall). Default: false."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
    ]
}

#[derive(Clone, Debug)]
pub(super) struct SearchRequest {
    kind: SearchKind,
    /// The canonical Chat working directory that result paths are shown relative to.
    display_base: PathBuf,
}

#[derive(Clone, Debug)]
enum SearchKind {
    /// A glob relative to the search root.
    Glob {
        pattern: String,
    },
    Grep(Box<GrepRequest>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

#[derive(Clone, Debug)]
pub(super) struct GrepRequest {
    pattern: String,
    globs: Vec<String>,
    file_type: Option<String>,
    output_mode: OutputMode,
    lines_before: usize,
    lines_after: usize,
    line_numbers: bool,
    case_insensitive: bool,
    multiline: bool,
    /// `None` applies the default limit; `Some(0)` lists everything.
    head_limit: Option<usize>,
    offset: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    glob: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_mode: Option<OutputMode>,
    #[serde(
        rename = "-B",
        default,
        deserialize_with = "semantic_integer",
        skip_serializing_if = "Option::is_none"
    )]
    lines_before: Option<usize>,
    #[serde(
        rename = "-A",
        default,
        deserialize_with = "semantic_integer",
        skip_serializing_if = "Option::is_none"
    )]
    lines_after: Option<usize>,
    #[serde(
        rename = "-C",
        default,
        deserialize_with = "semantic_integer",
        skip_serializing_if = "Option::is_none"
    )]
    context_alias: Option<usize>,
    #[serde(
        default,
        deserialize_with = "semantic_integer",
        skip_serializing_if = "Option::is_none"
    )]
    context: Option<usize>,
    #[serde(
        rename = "-n",
        default,
        deserialize_with = "optional_semantic_boolean",
        skip_serializing_if = "Option::is_none"
    )]
    line_numbers: Option<bool>,
    #[serde(
        rename = "-i",
        default,
        deserialize_with = "optional_semantic_boolean",
        skip_serializing_if = "Option::is_none"
    )]
    case_insensitive: Option<bool>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    file_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "semantic_integer",
        skip_serializing_if = "Option::is_none"
    )]
    head_limit: Option<usize>,
    #[serde(
        default,
        deserialize_with = "semantic_integer",
        skip_serializing_if = "Option::is_none"
    )]
    offset: Option<usize>,
    #[serde(
        default,
        deserialize_with = "optional_semantic_boolean",
        skip_serializing_if = "Option::is_none"
    )]
    multiline: Option<bool>,
}

fn optional_semantic_boolean<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    semantic_boolean(deserializer).map(Some)
}

pub(super) fn prepare_input(
    name: &str,
    input: &Value,
) -> Result<(String, Operation, Value), ToolError> {
    if name == GLOB_TOOL_NAME {
        prepare_glob(input)
    } else {
        prepare_grep(input)
    }
}

fn prepare_glob(input: &Value) -> Result<(String, Operation, Value), ToolError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GlobInput {
        pattern: String,
        path: Option<String>,
    }
    let parsed: GlobInput = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    let (root, pattern) = split_absolute_glob(&parsed.pattern).unwrap_or_else(|| {
        (
            parsed.path.unwrap_or_else(|| CURRENT_DIRECTORY.into()),
            parsed.pattern,
        )
    });
    OverrideBuilder::new(&root)
        .add(&pattern)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    Ok((
        root,
        Operation::Search(SearchRequest::new(SearchKind::Glob { pattern })),
        input.clone(),
    ))
}

/// Claude Code `extractGlobBaseDirectory`: ripgrep globs are relative to the searched directory,
/// so an absolute pattern searches the literal directory before its first glob character.
fn split_absolute_glob(pattern: &str) -> Option<(String, String)> {
    if !pattern.starts_with('/') {
        return None;
    }
    let literal_end = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    let separator = pattern[..literal_end].rfind('/')?;
    let base = if separator == 0 {
        "/"
    } else {
        &pattern[..separator]
    };
    Some((base.into(), pattern[separator + 1..].into()))
}

fn prepare_grep(input: &Value) -> Result<(String, Operation, Value), ToolError> {
    let parsed: GrepInput = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    let (lines_before, lines_after) = match parsed.context.or(parsed.context_alias) {
        Some(context) => (context, context),
        None => (
            parsed.lines_before.unwrap_or(0),
            parsed.lines_after.unwrap_or(0),
        ),
    };
    let request = GrepRequest {
        pattern: parsed.pattern.clone(),
        globs: parsed.glob.as_deref().map(split_globs).unwrap_or_default(),
        file_type: parsed.file_type.clone(),
        output_mode: parsed.output_mode.unwrap_or_default(),
        lines_before,
        lines_after,
        line_numbers: parsed.line_numbers.unwrap_or(true),
        case_insensitive: parsed.case_insensitive.unwrap_or(false),
        multiline: parsed.multiline.unwrap_or(false),
        head_limit: parsed.head_limit,
        offset: parsed.offset.unwrap_or(0),
    };
    request.matcher()?;
    let mut overrides = OverrideBuilder::new(CURRENT_DIRECTORY);
    for glob in &request.globs {
        overrides
            .add(glob)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    }
    if let Some(file_type) = &request.file_type {
        file_types(file_type)?;
    }
    let normalized_input = serde_json::to_value(&parsed)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    Ok((
        parsed.path.unwrap_or_else(|| CURRENT_DIRECTORY.into()),
        Operation::Search(SearchRequest::new(SearchKind::Grep(Box::new(request)))),
        normalized_input,
    ))
}

/// Claude Code splits `glob` on whitespace, then on commas unless a piece holds a brace group.
fn split_globs(glob: &str) -> Vec<String> {
    glob.split_whitespace()
        .flat_map(|piece| {
            if piece.contains('{') && piece.contains('}') {
                vec![piece]
            } else {
                piece.split(',').filter(|glob| !glob.is_empty()).collect()
            }
        })
        .map(String::from)
        .collect()
}

fn file_types(name: &str) -> Result<ignore::types::Types, ToolError> {
    TypesBuilder::new()
        .add_defaults()
        .select(name)
        .build()
        .map_err(|error| ToolError::InvalidInput(error.to_string()))
}

impl SearchRequest {
    fn new(kind: SearchKind) -> Self {
        Self {
            kind,
            display_base: PathBuf::new(),
        }
    }

    /// Checks the search root the way Claude Code's `validateInput` does, before any approval,
    /// and remembers the Chat working directory that result paths are shown relative to.
    pub(super) fn bind(
        &mut self,
        root: &Path,
        requested_path: &str,
        workdir: &Path,
    ) -> Result<(), ToolError> {
        let metadata = match std::fs::metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let missing = match self.kind {
                    SearchKind::Glob { .. } => "Directory does not exist",
                    SearchKind::Grep(_) => "Path does not exist",
                };
                return Err(ToolError::Rejected(format!(
                    "{missing}: {requested_path}. {FILE_NOT_FOUND_CWD_NOTE} {}.",
                    workdir.display()
                )));
            }
            Err(error) => return Err(io_error(root, error)),
        };
        if matches!(self.kind, SearchKind::Glob { .. }) && !metadata.is_dir() {
            return Err(ToolError::Rejected(format!(
                "Path is not a directory: {requested_path}"
            )));
        }
        self.display_base = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        Ok(())
    }
}

/// Claude Code `toRelativePath`: relative below the Chat working directory, absolute elsewhere.
fn display_path(path: &Path, workdir: &Path) -> PathBuf {
    path.strip_prefix(workdir).unwrap_or(path).to_path_buf()
}

impl GrepRequest {
    /// The matcher ripgrep builds for these flags: `^` and `$` match at line boundaries, and only
    /// multiline mode lets a match cross a line terminator, with `.` matching newlines there.
    fn matcher(&self) -> Result<RegexMatcher, ToolError> {
        let mut builder = RegexMatcherBuilder::new();
        builder
            .multi_line(true)
            .case_insensitive(self.case_insensitive)
            .ban_byte(Some(b'\0'));
        if self.multiline {
            builder.dot_matches_new_line(true);
        } else {
            builder.line_terminator(Some(b'\n'));
        }
        builder
            .build(&self.pattern)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))
    }

    fn searcher(&self) -> Searcher {
        let is_content = self.output_mode == OutputMode::Content;
        let mut builder = SearcherBuilder::new();
        builder
            .multi_line(self.multiline)
            .line_number(is_content && self.line_numbers);
        if is_content {
            builder
                .before_context(self.lines_before)
                .after_context(self.lines_after);
        }
        builder.build()
    }

    /// How many output entries decide the page, or `None` when every entry is needed.
    fn entries_needed(&self) -> Option<usize> {
        match self.head_limit {
            Some(0) => None,
            limit => Some(self.offset + limit.unwrap_or(DEFAULT_HEAD_LIMIT) + 1),
        }
    }
}

pub(super) fn execute(
    root: &Path,
    request: &SearchRequest,
    cancel: &CancellationToken,
) -> Result<ToolResult, ToolError> {
    let content = match &request.kind {
        SearchKind::Glob { pattern } => list_files(root, pattern, &request.display_base, cancel)?,
        SearchKind::Grep(grep) => search_contents(root, grep, &request.display_base, cancel)?,
    };
    Ok(ToolResult {
        content,
        is_error: false,
        output_ref: None,
    })
}

/// A walk that never follows symbolic links and prunes skipped names and sensitive paths below
/// the root; `respect_ignore_files` chooses between `rg` and `rg --no-ignore`, both `--hidden`.
fn walker(
    root: &Path,
    skipped_names: &'static [&'static str],
    respect_ignore_files: bool,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(respect_ignore_files)
        .hidden(false)
        .follow_links(false)
        .sort_by_file_name(OsStr::cmp)
        .filter_entry(move |entry| {
            let name = entry.file_name();
            !skipped_names.iter().any(|skipped| name == *skipped)
                && !is_sensitive_path(Path::new(name))
        });
    if respect_ignore_files {
        builder.add_custom_ignore_filename(RIPGREP_IGNORE_FILE);
    }
    builder
}

fn overrides(
    root: &Path,
    globs: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<ignore::overrides::Override, ToolError> {
    let mut builder = OverrideBuilder::new(root);
    for glob in globs {
        builder
            .add(glob.as_ref())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    }
    builder
        .build()
        .map_err(|error| ToolError::InvalidInput(error.to_string()))
}

/// `rg --files --glob <pattern> --sort=modified --no-ignore --hidden`: oldest first, the first
/// hundred.
fn list_files(
    root: &Path,
    pattern: &str,
    workdir: &Path,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    let mut walk = walker(root, GLOB_SKIPPED_NAMES, false);
    walk.overrides(overrides(root, [pattern])?);
    let mut files = Vec::new();
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            files.push((modified, entry.into_path()));
        }
    }
    if files.is_empty() {
        return Ok("No files found".into());
    }
    files.sort_by_key(|(modified, _)| *modified);
    let mut lines = files
        .iter()
        .take(MAX_GLOB_RESULTS)
        .map(|(_, path)| display_path(path, workdir).to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if files.len() > MAX_GLOB_RESULTS {
        lines.push(GLOB_TRUNCATED_NOTE.into());
    }
    Ok(lines.join("\n"))
}

fn search_contents(
    root: &Path,
    request: &GrepRequest,
    workdir: &Path,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    let matcher = request.matcher()?;
    let mut searcher = request.searcher();
    let mut walk = walker(root, GREP_SKIPPED_NAMES, true);
    walk.overrides(overrides(root, &request.globs)?);
    if let Some(file_type) = &request.file_type {
        walk.types(file_types(file_type)?);
    }
    let root_is_directory = std::fs::metadata(root)
        .map_err(|error| io_error(root, error))?
        .is_dir();
    let has_context = request.lines_before > 0 || request.lines_after > 0;
    let mut content_printer = StandardBuilder::new()
        .path(root_is_directory)
        .max_columns(Some(MAX_COLUMNS))
        .separator_search(has_context.then(|| CONTEXT_SEPARATOR.to_vec()))
        .build_no_color(Vec::new());
    let mut count_printer = SummaryBuilder::new()
        .kind(SummaryKind::Count)
        .build_no_color(Vec::new());
    let mut lines = Vec::new();
    let mut output_bytes = 0;
    let mut matched_files = Vec::new();
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let is_explicit = entry.depth() == 0;
        let path = entry.path();
        let Ok(file) = open_existing(path, false) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if metadata.len() > MAX_SEARCH_FILE_BYTES {
            if is_explicit {
                return Err(ToolError::Rejected(format!(
                    "{} is larger than 8 MiB, so {GREP_TOOL_NAME} does not search it. Use {READ_TOOL_NAME} with offset and limit to page through it.",
                    path.display()
                )));
            }
            continue;
        }
        // Like ripgrep, a file named explicitly reports binary matches while one found by the
        // walk stops at the first NUL byte.
        searcher.set_binary_detection(if is_explicit {
            BinaryDetection::convert(b'\0')
        } else {
            BinaryDetection::quit(b'\0')
        });
        let reader = file.take(MAX_SEARCH_FILE_BYTES);
        let shown_path = display_path(path, workdir);
        let printed = match request.output_mode {
            OutputMode::FilesWithMatches => {
                let mut printer = SummaryBuilder::new()
                    .kind(SummaryKind::PathWithMatch)
                    .build_no_color(std::io::sink());
                let mut sink = printer.sink_with_path(&matcher, path);
                if searcher.search_reader(&matcher, reader, &mut sink).is_ok() && sink.has_match() {
                    matched_files.push((metadata.modified().ok(), path.to_path_buf()));
                }
                continue;
            }
            OutputMode::Content => {
                let sink = content_printer.sink_with_path(&matcher, &shown_path);
                let _ = searcher.search_reader(&matcher, reader, sink);
                content_printer.get_mut().get_mut()
            }
            OutputMode::Count => {
                let sink = count_printer.sink_with_path(&matcher, &shown_path);
                let _ = searcher.search_reader(&matcher, reader, sink);
                count_printer.get_mut().get_mut()
            }
        };
        output_bytes += printed.len();
        lines.extend(
            String::from_utf8_lossy(printed)
                .split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .filter(|line| !line.is_empty())
                .map(String::from),
        );
        printed.clear();
        let has_page = request
            .entries_needed()
            .is_some_and(|needed| lines.len() >= needed);
        if has_page || output_bytes > MAX_SEARCH_OUTPUT_BYTES {
            break;
        }
    }
    Ok(match request.output_mode {
        OutputMode::Content => content_result(lines, request),
        OutputMode::Count => count_result(lines, request),
        OutputMode::FilesWithMatches => files_result(matched_files, request, workdir),
    })
}

/// Claude Code `applyHeadLimit`: skips `offset` entries, then keeps `head_limit` of them (250 when
/// unspecified, everything when 0). The limit is returned only when it dropped entries.
fn apply_head_limit<T>(items: Vec<T>, request: &GrepRequest) -> (Vec<T>, Option<usize>) {
    let mut kept = items.into_iter().skip(request.offset).collect::<Vec<_>>();
    if request.head_limit == Some(0) {
        return (kept, None);
    }
    let limit = request.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT);
    let applied_limit = (kept.len() > limit).then_some(limit);
    kept.truncate(limit);
    (kept, applied_limit)
}

/// Claude Code `formatLimitInfo`.
fn pagination(applied_limit: Option<usize>, offset: usize) -> String {
    let mut parts = Vec::new();
    if let Some(limit) = applied_limit {
        parts.push(format!("limit: {limit}"));
    }
    if offset > 0 {
        parts.push(format!("offset: {offset}"));
    }
    parts.join(", ")
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn content_result(lines: Vec<String>, request: &GrepRequest) -> String {
    let (lines, applied_limit) = apply_head_limit(lines, request);
    let content = if lines.is_empty() {
        "No matches found".into()
    } else {
        lines.join("\n")
    };
    match pagination(applied_limit, request.offset) {
        pagination if pagination.is_empty() => content,
        pagination => format!("{content}\n\n[Showing results with pagination = {pagination}]"),
    }
}

fn count_result(lines: Vec<String>, request: &GrepRequest) -> String {
    let (lines, applied_limit) = apply_head_limit(lines, request);
    let counts = lines
        .iter()
        .filter_map(|line| line.rsplit_once(':')?.1.parse::<usize>().ok())
        .collect::<Vec<_>>();
    let content = if lines.is_empty() {
        "No matches found".into()
    } else {
        lines.join("\n")
    };
    let pagination = match pagination(applied_limit, request.offset) {
        pagination if pagination.is_empty() => String::new(),
        pagination => format!(" with pagination = {pagination}"),
    };
    let occurrences: usize = counts.iter().sum();
    let files = counts.len();
    format!(
        "{content}\n\nFound {occurrences} total {} across {files} {}.{pagination}",
        plural(occurrences, "occurrence", "occurrences"),
        plural(files, "file", "files"),
    )
}

/// Newest first like Claude Code, with the path breaking ties.
fn files_result(
    mut matched_files: Vec<(Option<SystemTime>, PathBuf)>,
    request: &GrepRequest,
    workdir: &Path,
) -> String {
    matched_files.sort_by(|(left_time, left_path), (right_time, right_path)| {
        right_time
            .cmp(left_time)
            .then_with(|| left_path.cmp(right_path))
    });
    let (matched_files, applied_limit) = apply_head_limit(matched_files, request);
    if matched_files.is_empty() {
        return "No files found".into();
    }
    let pagination = match pagination(applied_limit, request.offset) {
        pagination if pagination.is_empty() => String::new(),
        pagination => format!(" {pagination}"),
    };
    let paths = matched_files
        .iter()
        .map(|(_, path)| display_path(path, workdir).to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    format!(
        "Found {} {}{pagination}\n{}",
        paths.len(),
        plural(paths.len(), "file", "files"),
        paths.join("\n")
    )
}
