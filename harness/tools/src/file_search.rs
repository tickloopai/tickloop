//! `Glob` and `Grep` over files given as a path below their root and their bytes, with the
//! ripgrep semantics of the local tools in `search.rs`: `ignore`'s override, gitignore and type
//! matchers with ripgrep's precedence of `.rgignore`, `.ignore`, `.gitignore` and
//! `.git/info/exclude`, hidden files included, the same skipped names and limits, and
//! `grep-searcher` for matching. An Execution Environment whose files are not on the local disk
//! answers the Port's `Glob` and `Grep` queries with it, so it never approximates them; the
//! structured results print as the tools print them through [`glob_text`] and [`grep_text`].

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    io,
    ops::ControlFlow,
    path::{Path, PathBuf},
    time::SystemTime,
};

use grep_printer::{SummaryBuilder, SummaryKind};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch,
};
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
    overrides::{Override, OverrideBuilder},
    types::{Types, TypesBuilder},
};
use serde::{Deserialize, Serialize};

use crate::is_sensitive_path;

/// Claude Code's `globLimits.maxResults` default.
pub const GLOB_RESULT_LIMIT: usize = 100;
const GLOB_TRUNCATED_NOTE: &str =
    "(Results are truncated. Consider using a more specific path or pattern.)";
/// Claude Code `DEFAULT_HEAD_LIMIT`: the entries Grep returns when `head_limit` is unspecified.
pub(crate) const DEFAULT_HEAD_LIMIT: usize = 250;
/// Claude Code passes `--max-columns 500` so minified or base64 lines do not flood the output.
pub(crate) const MAX_COLUMNS: u64 = 500;
pub(crate) const MAX_SEARCH_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Output collected from the files searched so far; like ripgrep's output buffer in Claude Code,
/// reaching it ends the search with the lines already found.
pub(crate) const MAX_SEARCH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const CONTEXT_SEPARATOR: &str = "--";
pub(crate) const RIPGREP_IGNORE_FILE: &str = ".rgignore";
const IGNORE_FILE: &str = ".ignore";
const GIT_IGNORE_FILE: &str = ".gitignore";
const GIT_DIRECTORY: &str = ".git";
const GIT_EXCLUDE_FILE: &str = ".git/info/exclude";
const JUJUTSU_DIRECTORY: &str = ".jj";
const BYTE_ORDER_MARK: &str = "\u{feff}";
/// Glob lists ignored and hidden files like `rg --files --no-ignore --hidden`, but never this
/// project's private data or Git's object store.
pub(crate) const GLOB_SKIPPED_NAMES: &[&str] = &[".git", ".tickloop"];
/// Claude Code `VCS_DIRECTORIES_TO_EXCLUDE`, plus this project's private data.
pub(crate) const GREP_SKIPPED_NAMES: &[&str] =
    &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl", ".tickloop"];
const OMITTED_MATCHING_LINE: &str = "[Omitted long matching line]";
const OMITTED_CONTEXT_LINE: &str = "[Omitted long context line]";
const NO_FILES_FOUND: &str = "No files found";
const NO_MATCHES_FOUND: &str = "No matches found";

/// A `Glob` below a searched directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobQuery {
    /// A glob relative to the searched directory.
    pub pattern: String,
    /// The most files returned, oldest first; [`GLOB_RESULT_LIMIT`] for the tool.
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

/// Every parameter of the `Grep` tool except its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepQuery {
    pub pattern: String,
    /// `rg --glob` filters, relative to the searched directory.
    pub globs: Vec<String>,
    /// An `rg --type` name.
    pub file_type: Option<String>,
    pub output_mode: GrepOutputMode,
    pub lines_before: usize,
    pub lines_after: usize,
    pub line_numbers: bool,
    pub case_insensitive: bool,
    pub multiline: bool,
    /// `None` applies the default limit; `Some(0)` lists everything.
    pub head_limit: Option<usize>,
    pub offset: usize,
}

/// One file to search: its path below the root, `/`-separated without a leading `/`.
#[derive(Clone, Copy, Debug)]
pub struct SearchedFile<'a> {
    pub path: &'a str,
    pub content: &'a [u8],
    pub modified: Option<SystemTime>,
}

/// A file found by a search, by its path below the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundFile {
    pub path: String,
    pub modified: Option<SystemTime>,
}

/// The files a `Glob` matched, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobMatches {
    pub files: Vec<FoundFile>,
    /// More files matched than the query's limit.
    pub truncated: bool,
}

/// The page of `Grep` output the query's `offset` and `head_limit` select.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatches {
    pub output: GrepOutput,
    /// The head limit dropped entries after this page.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrepOutput {
    /// The lines ripgrep prints, in order.
    Content(Vec<GrepLine>),
    /// Matching files, newest first with the path breaking ties.
    Files(Vec<FoundFile>),
    /// Matching lines per file (matches in multiline mode), in walk order.
    Counts(Vec<FileMatchCount>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatchCount {
    pub path: String,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrepLine {
    /// A matching line; a multiline match gives one per line.
    Match(GrepLineText),
    Context(GrepLineText),
    /// The `--` between groups of lines that are not adjacent, and between files when context
    /// was requested.
    Break,
    /// ripgrep's notice on a binary file that matched.
    Binary(BinaryMatch),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepLineText {
    /// The file below the root; `None` when the search named that one file.
    pub path: Option<String>,
    pub line_number: Option<u64>,
    /// The line without its terminator; `None` when it is longer than ripgrep shows.
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryMatch {
    /// The file below the root; `None` when the search named that one file.
    pub path: Option<String>,
    pub offset: u64,
    /// The walk found the file and stopped searching it at the binary data; a named file is
    /// searched whole and reported only as matching.
    pub stopped_after_match: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FileSearchError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0} does not exist")]
    NotFound(String),
    #[error("{0} is not a directory")]
    NotADirectory(String),
    #[error("{0} is larger than 8 MiB")]
    TooLarge(String),
}

impl GrepQuery {
    /// The matcher ripgrep builds for these flags: `^` and `$` match at line boundaries, and only
    /// multiline mode lets a match cross a line terminator, with `.` matching newlines there.
    pub(crate) fn matcher(&self) -> Result<RegexMatcher, FileSearchError> {
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
            .map_err(|error| FileSearchError::InvalidInput(error.to_string()))
    }

    pub(crate) fn searcher(&self) -> Searcher {
        let is_content = self.output_mode == GrepOutputMode::Content;
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
    pub(crate) fn entries_needed(&self) -> Option<usize> {
        match self.head_limit {
            Some(0) => None,
            limit => Some(self.offset + limit.unwrap_or(DEFAULT_HEAD_LIMIT) + 1),
        }
    }

    pub(crate) fn has_context(&self) -> bool {
        self.lines_before > 0 || self.lines_after > 0
    }

    fn page_limit(&self) -> usize {
        self.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT)
    }
}

/// Like ripgrep, a file named explicitly reports binary matches while one found by the walk stops
/// at the first NUL byte.
pub(crate) fn binary_detection(is_explicit: bool) -> BinaryDetection {
    if is_explicit {
        BinaryDetection::convert(b'\0')
    } else {
        BinaryDetection::quit(b'\0')
    }
}

pub(crate) fn file_types(name: &str) -> Result<Types, FileSearchError> {
    TypesBuilder::new()
        .add_defaults()
        .select(name)
        .build()
        .map_err(|error| FileSearchError::InvalidInput(error.to_string()))
}

pub(crate) fn override_matcher(
    root: &Path,
    globs: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<Override, FileSearchError> {
    let mut builder = OverrideBuilder::new(root);
    for glob in globs {
        builder
            .add(glob.as_ref())
            .map_err(|error| FileSearchError::InvalidInput(error.to_string()))?;
    }
    builder
        .build()
        .map_err(|error| FileSearchError::InvalidInput(error.to_string()))
}

/// Claude Code `applyHeadLimit`: skips `offset` entries, then keeps `head_limit` of them (250 when
/// unspecified, everything when 0). The limit is returned only when it dropped entries.
pub(crate) fn apply_head_limit<T>(items: Vec<T>, query: &GrepQuery) -> (Vec<T>, Option<usize>) {
    let mut kept = items.into_iter().skip(query.offset).collect::<Vec<_>>();
    if query.head_limit == Some(0) {
        return (kept, None);
    }
    let limit = query.page_limit();
    let applied_limit = (kept.len() > limit).then_some(limit);
    kept.truncate(limit);
    (kept, applied_limit)
}

/// `rg --files --glob <pattern> --sort=modified --no-ignore --hidden` below `base`, a directory
/// path below the root (empty for the root itself): oldest first, at most `query.limit` files.
pub fn glob_files<'a>(
    files: impl IntoIterator<Item = SearchedFile<'a>>,
    base: &str,
    query: &GlobQuery,
) -> Result<GlobMatches, FileSearchError> {
    let overrides = override_matcher(&virtual_path(base), [&query.pattern])?;
    let tree = FileTree::new(files);
    if let SearchBase::File(_) = tree.base(base)? {
        return Err(FileSearchError::NotADirectory(base.into()));
    }
    let mut walk = Walk {
        tree: &tree,
        base,
        overrides,
        types: None,
        ignore_files: None,
        skipped_names: GLOB_SKIPPED_NAMES,
        skipped_directories: HashMap::new(),
    };
    let found = walk
        .files()
        .into_iter()
        .map(|file| FoundFile {
            path: file.path.into(),
            modified: file.modified,
        })
        .collect();
    Ok(oldest_first(found, query.limit))
}

/// Files found in walk order as `rg --sort=modified` lists them: oldest first, the walk order
/// breaking ties, at most `limit` of them.
pub(crate) fn oldest_first(mut found: Vec<FoundFile>, limit: usize) -> GlobMatches {
    found.sort_by_key(|file| file.modified);
    let truncated = found.len() > limit;
    found.truncate(limit);
    GlobMatches {
        files: found,
        truncated,
    }
}

/// `rg` with the query's flags on `base`, a file or directory path below the root (empty for the
/// root itself), returning the page the query's `offset` and `head_limit` select.
pub fn grep_files<'a>(
    files: impl IntoIterator<Item = SearchedFile<'a>>,
    base: &str,
    query: &GrepQuery,
) -> Result<GrepMatches, FileSearchError> {
    let mut search = GrepSearch::new(query)?;
    let overrides = override_matcher(&virtual_path(base), &query.globs)?;
    let types = query.file_type.as_deref().map(file_types).transpose()?;
    let tree = FileTree::new(files);
    let (searched, shows_paths) = match tree.base(base)? {
        SearchBase::File(index) => (vec![(&tree.files[index], true)], false),
        SearchBase::Directory => (
            Walk {
                tree: &tree,
                base,
                overrides,
                types,
                ignore_files: Some(IgnoreFiles::default()),
                skipped_names: GREP_SKIPPED_NAMES,
                skipped_directories: HashMap::new(),
            }
            .files()
            .into_iter()
            .map(|file| (file, false))
            .collect(),
            true,
        ),
    };
    for (file, is_explicit) in searched {
        let found = search.search(
            file.path,
            file.content,
            file.content.len() as u64,
            file.modified,
            is_explicit,
            shows_paths,
        )?;
        if found.is_break() {
            break;
        }
    }
    Ok(search.finish())
}

/// One `rg` run over files given one at a time in walk order: the matcher, searcher and printers
/// it keeps across files, and what it found so far.
pub(crate) struct GrepSearch<'q> {
    query: &'q GrepQuery,
    matcher: RegexMatcher,
    searcher: Searcher,
    content: ContentLines,
    counts: Vec<FileMatchCount>,
    count_bytes: usize,
    matched_files: Vec<FoundFile>,
}

impl<'q> GrepSearch<'q> {
    pub(crate) fn new(query: &'q GrepQuery) -> Result<Self, FileSearchError> {
        let matcher = query.matcher()?;
        let searcher = query.searcher();
        let content = ContentLines {
            lines: Vec::new(),
            ever_written: false,
            output_bytes: 0,
            separates_searches: query.has_context(),
            is_multi_line: searcher.multi_line_with_matcher(&matcher),
        };
        Ok(Self {
            query,
            matcher,
            searcher,
            content,
            counts: Vec::new(),
            count_bytes: 0,
            matched_files: Vec::new(),
        })
    }

    /// Searches the file at `path` below the root, `size` bytes that `content` reads; the search
    /// named it when `is_explicit`, and its lines show their file when `shows_paths`. Breaks once
    /// the page is complete or the output is full.
    pub(crate) fn search(
        &mut self,
        path: &str,
        content: impl io::Read,
        size: u64,
        modified: Option<SystemTime>,
        is_explicit: bool,
        shows_paths: bool,
    ) -> Result<ControlFlow<()>, FileSearchError> {
        if size > MAX_SEARCH_FILE_BYTES {
            if is_explicit {
                return Err(FileSearchError::TooLarge(path.into()));
            }
            return Ok(ControlFlow::Continue(()));
        }
        let content = content.take(MAX_SEARCH_FILE_BYTES);
        let matcher = &self.matcher;
        self.searcher
            .set_binary_detection(binary_detection(is_explicit));
        let (entries, output_bytes) = match self.query.output_mode {
            GrepOutputMode::FilesWithMatches => {
                let mut printer = SummaryBuilder::new()
                    .kind(SummaryKind::PathWithMatch)
                    .build_no_color(io::sink());
                let mut sink = printer.sink_with_path(matcher, Path::new(path));
                if self
                    .searcher
                    .search_reader(matcher, content, &mut sink)
                    .is_ok()
                    && sink.has_match()
                {
                    self.matched_files.push(FoundFile {
                        path: path.into(),
                        modified,
                    });
                }
                return Ok(ControlFlow::Continue(()));
            }
            GrepOutputMode::Content => {
                let sink = ContentSink {
                    output: &mut self.content,
                    path: shows_paths.then_some(path),
                    has_written: false,
                    has_match: false,
                    binary_offset: None,
                };
                let _ = self.searcher.search_reader(matcher, content, sink);
                (self.content.lines.len(), self.content.output_bytes)
            }
            GrepOutputMode::Count => {
                let mut printer = SummaryBuilder::new()
                    .kind(SummaryKind::Count)
                    .build_no_color(Vec::new());
                let sink = printer.sink_with_path(matcher, Path::new(path));
                let _ = self.searcher.search_reader(matcher, content, sink);
                let printed = printer.get_mut().get_ref();
                self.count_bytes += printed.len();
                if let Some(count) = String::from_utf8_lossy(printed)
                    .trim_end()
                    .rsplit_once(':')
                    .and_then(|(_, count)| count.parse().ok())
                {
                    self.counts.push(FileMatchCount {
                        path: path.into(),
                        count,
                    });
                }
                (self.counts.len(), self.count_bytes)
            }
        };
        let has_page = self
            .query
            .entries_needed()
            .is_some_and(|needed| entries >= needed);
        Ok(if has_page || output_bytes > MAX_SEARCH_OUTPUT_BYTES {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    }

    pub(crate) fn finish(self) -> GrepMatches {
        let query = self.query;
        let (output, applied_limit) = match query.output_mode {
            GrepOutputMode::Content => {
                let (lines, applied_limit) = apply_head_limit(self.content.lines, query);
                (GrepOutput::Content(lines), applied_limit)
            }
            GrepOutputMode::Count => {
                let (counts, applied_limit) = apply_head_limit(self.counts, query);
                (GrepOutput::Counts(counts), applied_limit)
            }
            GrepOutputMode::FilesWithMatches => {
                let mut matched_files = self.matched_files;
                matched_files.sort_by(|left, right| {
                    right
                        .modified
                        .cmp(&left.modified)
                        .then_with(|| walk_order(&left.path, &right.path))
                });
                let (files, applied_limit) = apply_head_limit(matched_files, query);
                (GrepOutput::Files(files), applied_limit)
            }
        };
        GrepMatches {
            output,
            truncated: applied_limit.is_some(),
        }
    }
}

/// The text the `Glob` tool returns for these matches; `display` shows a path below the root.
pub fn glob_text(matches: &GlobMatches, display: impl Fn(&str) -> String) -> String {
    glob_listing(
        matches
            .files
            .iter()
            .map(|file| display(&file.path))
            .collect(),
        matches.truncated,
    )
}

/// The text the `Grep` tool returns for the page of `query` in these matches; `display` shows a
/// path below the root.
pub fn grep_text(
    matches: &GrepMatches,
    query: &GrepQuery,
    display: impl Fn(&str) -> String,
) -> String {
    let applied_limit = matches.truncated.then(|| query.page_limit());
    match &matches.output {
        GrepOutput::Content(lines) => content_text(
            lines.iter().map(|line| line_text(line, &display)).collect(),
            applied_limit,
            query.offset,
        ),
        GrepOutput::Counts(counts) => count_text(
            counts
                .iter()
                .map(|count| format!("{}:{}", display(&count.path), count.count))
                .collect(),
            applied_limit,
            query.offset,
        ),
        GrepOutput::Files(files) => files_text(
            files.iter().map(|file| display(&file.path)).collect(),
            applied_limit,
            query.offset,
        ),
    }
}

/// One output line as ripgrep's standard printer writes it without colors.
fn line_text(line: &GrepLine, display: &impl Fn(&str) -> String) -> String {
    let prefixed = |text: &GrepLineText, separator: char, omitted: &str| {
        let mut printed = String::new();
        if let Some(path) = &text.path {
            printed.push_str(&display(path));
            printed.push(separator);
        }
        if let Some(line_number) = text.line_number {
            printed.push_str(&line_number.to_string());
            printed.push(separator);
        }
        printed.push_str(text.text.as_deref().unwrap_or(omitted));
        printed
    };
    match line {
        GrepLine::Match(text) => prefixed(text, ':', OMITTED_MATCHING_LINE),
        GrepLine::Context(text) => prefixed(text, '-', OMITTED_CONTEXT_LINE),
        GrepLine::Break => CONTEXT_SEPARATOR.into(),
        GrepLine::Binary(binary) => {
            let notice = if binary.stopped_after_match {
                "WARNING: stopped searching binary file after match"
            } else {
                "binary file matches"
            };
            let path = binary
                .path
                .as_deref()
                .map(|path| format!("{}: ", display(path)))
                .unwrap_or_default();
            format!(
                "{path}{notice} (found \"\\0\" byte around offset {})",
                binary.offset
            )
        }
    }
}

pub(crate) fn glob_listing(paths: Vec<String>, truncated: bool) -> String {
    if paths.is_empty() {
        return NO_FILES_FOUND.into();
    }
    let mut lines = paths;
    if truncated {
        lines.push(GLOB_TRUNCATED_NOTE.into());
    }
    lines.join("\n")
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

pub(crate) fn content_text(
    lines: Vec<String>,
    applied_limit: Option<usize>,
    offset: usize,
) -> String {
    let content = if lines.is_empty() {
        NO_MATCHES_FOUND.into()
    } else {
        lines.join("\n")
    };
    match pagination(applied_limit, offset) {
        pagination if pagination.is_empty() => content,
        pagination => format!("{content}\n\n[Showing results with pagination = {pagination}]"),
    }
}

/// `lines` are `path:count` entries.
pub(crate) fn count_text(
    lines: Vec<String>,
    applied_limit: Option<usize>,
    offset: usize,
) -> String {
    let counts = lines
        .iter()
        .filter_map(|line| line.rsplit_once(':')?.1.parse::<usize>().ok())
        .collect::<Vec<_>>();
    let content = if lines.is_empty() {
        NO_MATCHES_FOUND.into()
    } else {
        lines.join("\n")
    };
    let pagination = match pagination(applied_limit, offset) {
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

pub(crate) fn files_text(
    paths: Vec<String>,
    applied_limit: Option<usize>,
    offset: usize,
) -> String {
    if paths.is_empty() {
        return NO_FILES_FOUND.into();
    }
    let pagination = match pagination(applied_limit, offset) {
        pagination if pagination.is_empty() => String::new(),
        pagination => format!(" {pagination}"),
    };
    format!(
        "Found {} {}{pagination}\n{}",
        paths.len(),
        plural(paths.len(), "file", "files"),
        paths.join("\n")
    )
}

/// The order ripgrep's walk sorted by file name visits paths in: component by component.
fn walk_order(left: &str, right: &str) -> Ordering {
    left.split('/').cmp(right.split('/'))
}

/// The absolute form the `ignore` matchers see for a path below the root; nothing reads it.
fn virtual_path(path: &str) -> PathBuf {
    PathBuf::from(format!("/{path}"))
}

fn child(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.into()
    } else {
        format!("{directory}/{name}")
    }
}

fn parent(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(path.rsplit_once('/').map_or("", |(parent, _)| parent))
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

enum SearchBase {
    File(usize),
    Directory,
}

/// The searched files in walk order, with the directories their paths imply.
struct FileTree<'a> {
    files: Vec<SearchedFile<'a>>,
    by_path: HashMap<&'a str, usize>,
    directories: HashSet<&'a str>,
}

impl<'a> FileTree<'a> {
    fn new(files: impl IntoIterator<Item = SearchedFile<'a>>) -> Self {
        let mut files = files.into_iter().collect::<Vec<_>>();
        files.sort_by(|left, right| walk_order(left.path, right.path));
        let by_path = files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.path, index))
            .collect();
        let mut directories = HashSet::from([""]);
        for file in &files {
            let mut path = file.path;
            while let Some(directory) = parent(path) {
                directories.insert(directory);
                path = directory;
            }
        }
        Self {
            files,
            by_path,
            directories,
        }
    }

    fn base(&self, base: &str) -> Result<SearchBase, FileSearchError> {
        if let Some(&index) = self.by_path.get(base) {
            Ok(SearchBase::File(index))
        } else if self.directories.contains(base) {
            Ok(SearchBase::Directory)
        } else {
            Err(FileSearchError::NotFound(base.into()))
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.by_path.contains_key(path) || self.directories.contains(path)
    }

    fn content(&self, path: &str) -> Option<&'a [u8]> {
        self.by_path
            .get(path)
            .map(|&index| self.files[index].content)
    }
}

/// ripgrep's walk below a directory: entries skipped by overrides, ignore files, types or the
/// skipped names, and every directory below a skipped one, are left out.
struct Walk<'t, 'a> {
    tree: &'t FileTree<'a>,
    base: &'t str,
    overrides: Override,
    types: Option<Types>,
    /// `None` searches like `--no-ignore`.
    ignore_files: Option<IgnoreFiles>,
    skipped_names: &'static [&'static str],
    skipped_directories: HashMap<String, bool>,
}

impl<'t, 'a> Walk<'t, 'a> {
    fn files(&mut self) -> Vec<&'t SearchedFile<'a>> {
        let tree = self.tree;
        let below = if self.base.is_empty() {
            String::new()
        } else {
            format!("{}/", self.base)
        };
        tree.files
            .iter()
            .filter(|file| file.path.starts_with(&below))
            .filter(|file| {
                !parent(file.path).is_some_and(|directory| self.is_directory_skipped(directory))
                    && !self.is_entry_skipped(file.path, false)
            })
            .collect()
    }

    fn is_directory_skipped(&mut self, directory: &str) -> bool {
        if directory == self.base {
            return false;
        }
        if let Some(&skipped) = self.skipped_directories.get(directory) {
            return skipped;
        }
        let skipped = parent(directory).is_some_and(|above| self.is_directory_skipped(above))
            || self.is_entry_skipped(directory, true);
        self.skipped_directories.insert(directory.into(), skipped);
        skipped
    }

    /// `ignore`'s precedence: an override decides alone, then ignore files and types; the entry
    /// filter for skipped and sensitive names applies to every entry.
    fn is_entry_skipped(&mut self, path: &str, is_dir: bool) -> bool {
        let name = file_name(path);
        if self.skipped_names.contains(&name) || is_sensitive_path(Path::new(name)) {
            return true;
        }
        let matched_path = virtual_path(path);
        match self.overrides.matched(&matched_path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
        if let Some(ignore_files) = &mut self.ignore_files
            && ignore_files.is_ignored(self.tree, path, is_dir)
        {
            return true;
        }
        self.types
            .as_ref()
            .is_some_and(|types| types.matched(&matched_path, is_dir).is_ignore())
    }
}

/// The ignore files of each directory, read on first use.
#[derive(Default)]
struct IgnoreFiles {
    directories: HashMap<String, DirectoryIgnores>,
}

struct DirectoryIgnores {
    ripgrep: Gitignore,
    ignore: Gitignore,
    git: Gitignore,
    git_exclude: Gitignore,
    has_repository: bool,
}

impl IgnoreFiles {
    /// `ignore`'s `matched_ignore`: the nearest directory's rule wins within each kind of ignore
    /// file, `.gitignore` and `.git/info/exclude` count only inside a repository and stop at its
    /// top, and `.rgignore` beats `.ignore`, which beats Git's files.
    fn is_ignored(&mut self, tree: &FileTree<'_>, path: &str, is_dir: bool) -> bool {
        let mut directories = Vec::new();
        let mut current = path;
        while let Some(directory) = parent(current) {
            directories.push(directory);
            current = directory;
        }
        for directory in &directories {
            if !self.directories.contains_key(*directory) {
                let ignores = DirectoryIgnores::read(tree, directory);
                self.directories.insert((*directory).into(), ignores);
            }
        }
        let chain = directories
            .iter()
            .map(|directory| &self.directories[*directory])
            .collect::<Vec<_>>();
        let in_repository = chain.iter().any(|ignores| ignores.has_repository);
        let matched_path = virtual_path(path);
        let (mut ripgrep, mut ignore, mut git, mut git_exclude) =
            (Match::None, Match::None, Match::None, Match::None);
        let mut passed_repository_top = false;
        for ignores in chain {
            if ripgrep.is_none() {
                ripgrep = ignores.ripgrep.matched(&matched_path, is_dir);
            }
            if ignore.is_none() {
                ignore = ignores.ignore.matched(&matched_path, is_dir);
            }
            if in_repository && !passed_repository_top {
                if git.is_none() {
                    git = ignores.git.matched(&matched_path, is_dir);
                }
                if git_exclude.is_none() {
                    git_exclude = ignores.git_exclude.matched(&matched_path, is_dir);
                }
            }
            passed_repository_top |= ignores.has_repository;
        }
        ripgrep.or(ignore).or(git).or(git_exclude).is_ignore()
    }
}

impl DirectoryIgnores {
    fn read(tree: &FileTree<'_>, directory: &str) -> Self {
        let git_directory = child(directory, GIT_DIRECTORY);
        let git_exclude = if tree.directories.contains(git_directory.as_str()) {
            gitignore(tree, directory, &child(directory, GIT_EXCLUDE_FILE))
        } else {
            Gitignore::empty()
        };
        Self {
            ripgrep: gitignore(tree, directory, &child(directory, RIPGREP_IGNORE_FILE)),
            ignore: gitignore(tree, directory, &child(directory, IGNORE_FILE)),
            git: gitignore(tree, directory, &child(directory, GIT_IGNORE_FILE)),
            git_exclude,
            has_repository: tree.exists(&git_directory)
                || tree.exists(&child(directory, JUJUTSU_DIRECTORY)),
        }
    }
}

/// `GitignoreBuilder::add` on a file's bytes: lines as `BufRead::lines` gives them, stopping at
/// the first one that is not UTF-8, with a byte order mark dropped from the first.
fn gitignore(tree: &FileTree<'_>, directory: &str, file: &str) -> Gitignore {
    let Some(content) = tree.content(file) else {
        return Gitignore::empty();
    };
    let mut builder = GitignoreBuilder::new(virtual_path(directory));
    let source = virtual_path(file);
    let mut lines = content.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let ends_with_newline = content.ends_with(b"\n");
    let last = lines.len().saturating_sub(1);
    for (index, line) in lines.into_iter().enumerate() {
        let terminated = index < last || ends_with_newline;
        let line = match line.strip_suffix(b"\r") {
            Some(stripped) if terminated => stripped,
            _ => line,
        };
        let Ok(line) = std::str::from_utf8(line) else {
            break;
        };
        let line = if index == 0 {
            line.trim_start_matches(BYTE_ORDER_MARK)
        } else {
            line
        };
        let _ = builder.add_line(Some(source.clone()), line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The content lines of every searched file, with what ripgrep's standard printer keeps across
/// files: whether anything was printed yet and how many bytes.
struct ContentLines {
    lines: Vec<GrepLine>,
    ever_written: bool,
    output_bytes: usize,
    separates_searches: bool,
    is_multi_line: bool,
}

impl ContentLines {
    fn push(&mut self, line: GrepLine) {
        self.ever_written = true;
        let printed = line_text(&line, &str::to_owned);
        if printed.is_empty() {
            return;
        }
        self.output_bytes += printed.len() + 1;
        self.lines.push(line);
    }
}

/// ripgrep's standard printer for one file, recording lines instead of printing them.
struct ContentSink<'c> {
    output: &'c mut ContentLines,
    path: Option<&'c str>,
    has_written: bool,
    has_match: bool,
    binary_offset: Option<u64>,
}

impl ContentSink<'_> {
    fn is_stopped(&self, searcher: &Searcher) -> bool {
        searcher.binary_detection().convert_byte().is_some() && self.binary_offset.is_some()
    }

    fn push(&mut self, line: GrepLine) {
        if !self.has_written && self.output.separates_searches && self.output.ever_written {
            self.output.push(GrepLine::Break);
        }
        self.has_written = true;
        self.output.push(line);
    }

    fn text(&self, line: &[u8], line_number: Option<u64>) -> GrepLineText {
        let text = (line.len() as u64 <= MAX_COLUMNS).then(|| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            String::from_utf8_lossy(line).into_owned()
        });
        GrepLineText {
            path: self.path.map(str::to_owned),
            line_number,
            text,
        }
    }
}

impl Sink for ContentSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, searcher: &Searcher, found: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.has_match = true;
        if self.is_stopped(searcher) {
            return Ok(false);
        }
        if self.output.is_multi_line {
            for (index, line) in found.lines().enumerate() {
                let line_number = found.line_number().map(|number| number + index as u64);
                let text = self.text(line, line_number);
                self.push(GrepLine::Match(text));
            }
        } else {
            let text = self.text(found.bytes(), found.line_number());
            self.push(GrepLine::Match(text));
        }
        Ok(true)
    }

    fn context(
        &mut self,
        searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        if self.is_stopped(searcher) {
            return Ok(false);
        }
        let text = self.text(context.bytes(), context.line_number());
        self.push(GrepLine::Context(text));
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, io::Error> {
        self.has_written = true;
        self.output.push(GrepLine::Break);
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, offset: u64) -> Result<bool, io::Error> {
        self.binary_offset = Some(offset);
        Ok(true)
    }

    fn finish(&mut self, searcher: &Searcher, _: &SinkFinish) -> Result<(), io::Error> {
        if let Some(offset) = self.binary_offset
            && self.has_match
        {
            self.has_written = true;
            self.output.push(GrepLine::Binary(BinaryMatch {
                path: self.path.map(str::to_owned),
                offset,
                stopped_after_match: searcher.binary_detection().quit_byte().is_some(),
            }));
        }
        Ok(())
    }
}
