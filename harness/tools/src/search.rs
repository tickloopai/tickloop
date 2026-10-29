//! Claude Code's `Glob` and `Grep` tools (`GlobTool.ts`, `utils/glob.ts`, `GrepTool.ts`) on
//! ripgrep's own libraries: `ignore` walks the tree the way `rg --files` does, and
//! `grep-searcher` with `grep-printer` produce the lines `rg` would print.

use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use super::{
    Operation, READ_TOOL_NAME, ToolDefinition, ToolError,
    chat_files::{FileTarget, tool_error},
    file_port::{FileInspection, FileKind, FilePath, FileQuery, error_code},
    file_search::{
        FileSearchError, GLOB_RESULT_LIMIT, GlobQuery, GrepLine, GrepOutput, GrepOutputMode,
        GrepQuery, file_types, glob_text, grep_text, override_matcher,
    },
    is_sensitive_path,
    read::semantic_integer,
    shell::BASH_TOOL_NAME,
    write::{FILE_NOT_FOUND_CWD_NOTE, semantic_boolean},
};
use tickloop_harness_protocol::ToolResult;

pub const GLOB_TOOL_NAME: &str = "Glob";
pub const GREP_TOOL_NAME: &str = "Grep";

const CURRENT_DIRECTORY: &str = ".";
const FILESYSTEM_ROOT: &str = "/";

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
    Grep(Box<GrepQuery>),
}

fn invalid_input(error: FileSearchError) -> ToolError {
    ToolError::InvalidInput(error.to_string())
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
    output_mode: Option<GrepOutputMode>,
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
    let (path, request, normalized_input) = grep_query(input)?;
    Ok((
        path,
        Operation::Search(SearchRequest::new(SearchKind::Grep(Box::new(request)))),
        normalized_input,
    ))
}

/// The searched path, the checked query and the normalized input of a `Grep` call.
pub(crate) fn grep_query(input: &Value) -> Result<(String, GrepQuery, Value), ToolError> {
    let parsed: GrepInput = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    let (lines_before, lines_after) = match parsed.context.or(parsed.context_alias) {
        Some(context) => (context, context),
        None => (
            parsed.lines_before.unwrap_or(0),
            parsed.lines_after.unwrap_or(0),
        ),
    };
    let request = GrepQuery {
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
    request.matcher().map_err(invalid_input)?;
    override_matcher(Path::new(CURRENT_DIRECTORY), &request.globs).map_err(invalid_input)?;
    if let Some(file_type) = &request.file_type {
        file_types(file_type).map_err(invalid_input)?;
    }
    let normalized_input = serde_json::to_value(&parsed)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    Ok((
        parsed.path.unwrap_or_else(|| CURRENT_DIRECTORY.into()),
        request,
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
        root: &FileTarget,
        requested_path: &str,
        workdir: &Path,
        display_base: PathBuf,
    ) -> Result<(), ToolError> {
        let Some(kind) = root.kind else {
            let missing = match self.kind {
                SearchKind::Glob { .. } => "Directory does not exist",
                SearchKind::Grep(_) => "Path does not exist",
            };
            return Err(ToolError::Rejected(format!(
                "{missing}: {requested_path}. {FILE_NOT_FOUND_CWD_NOTE} {}.",
                workdir.display()
            )));
        };
        if matches!(self.kind, SearchKind::Glob { .. }) && kind != FileKind::Directory {
            return Err(ToolError::Rejected(format!(
                "Path is not a directory: {requested_path}"
            )));
        }
        self.display_base = display_base;
        Ok(())
    }
}

/// Claude Code `toRelativePath`: relative below the Chat working directory, absolute elsewhere.
fn display_path(path: &Path, workdir: &Path) -> PathBuf {
    path.strip_prefix(workdir).unwrap_or(path).to_path_buf()
}

/// Runs the search where the searched path's host keeps it and prints the found paths the way
/// Claude Code does, leaving out any below the searched path that look like credentials.
pub(super) async fn execute(
    root: &FileTarget,
    request: &SearchRequest,
) -> Result<ToolResult, ToolError> {
    let root_display = match root.path.root() {
        Some(name) => PathBuf::from(
            root.host
                .capabilities()
                .display(&FilePath::in_root(name.clone(), "").expect("a root's own path")),
        ),
        None => PathBuf::from(FILESYSTEM_ROOT),
    };
    let display = |below: &str| {
        display_path(&root_display.join(below), &request.display_base)
            .to_string_lossy()
            .into_owned()
    };
    let searched = root.path.path().trim_start_matches('/');
    let is_hidden = |below: &str| {
        let below = below.trim_start_matches('/');
        let inside = below
            .strip_prefix(searched)
            .filter(|rest| searched.is_empty() || rest.is_empty() || rest.starts_with('/'))
            .unwrap_or(below);
        is_sensitive_path(Path::new(inside))
    };
    let content = match &request.kind {
        SearchKind::Glob { pattern } => {
            let query = GlobQuery {
                pattern: pattern.clone(),
                limit: GLOB_RESULT_LIMIT,
            };
            let FileInspection::Glob(mut matches) = root
                .inspect(FileQuery::Glob {
                    path: root.path.clone(),
                    query,
                })
                .await?
            else {
                return Err(unexpected_answer(root));
            };
            matches.files.retain(|file| !is_hidden(&file.path));
            glob_text(&matches, display)
        }
        SearchKind::Grep(query) => {
            let inspection = root
                .host
                .inspect(FileQuery::Grep {
                    path: root.path.clone(),
                    query: (**query).clone(),
                })
                .await
                .map_err(|error| match error.code.as_str() {
                    error_code::TOO_LARGE => ToolError::Rejected(format!(
                        "{}, so {GREP_TOOL_NAME} does not search it. Use {READ_TOOL_NAME} with offset and limit to page through it.",
                        error.message
                    )),
                    _ => tool_error(error, &root.shown),
                })?;
            let FileInspection::Grep(mut matches) = inspection else {
                return Err(unexpected_answer(root));
            };
            hide_grep_output(&mut matches.output, is_hidden);
            grep_text(&matches, query, display)
        }
    };
    Ok(ToolResult {
        content,
        is_error: false,
        output_ref: None,
    })
}

/// Leaves out what the search found in files `is_hidden` names, and the group separators that
/// would then stand next to each other or at either end.
fn hide_grep_output(output: &mut GrepOutput, is_hidden: impl Fn(&str) -> bool) {
    let shown = |path: &Option<String>| path.as_deref().is_none_or(|path| !is_hidden(path));
    match output {
        GrepOutput::Files(files) => files.retain(|file| !is_hidden(&file.path)),
        GrepOutput::Counts(counts) => counts.retain(|count| !is_hidden(&count.path)),
        GrepOutput::Content(lines) => {
            lines.retain(|line| match line {
                GrepLine::Match(text) | GrepLine::Context(text) => shown(&text.path),
                GrepLine::Binary(binary) => shown(&binary.path),
                GrepLine::Break => true,
            });
            lines.dedup_by(|next, previous| {
                matches!(next, GrepLine::Break) && matches!(previous, GrepLine::Break)
            });
            if matches!(lines.first(), Some(GrepLine::Break)) {
                lines.remove(0);
            }
            if matches!(lines.last(), Some(GrepLine::Break)) {
                lines.pop();
            }
        }
    }
}

fn unexpected_answer(root: &FileTarget) -> ToolError {
    ToolError::Io {
        path: root.shown.clone(),
        reason: "the environment answered a search with something else".into(),
    }
}
